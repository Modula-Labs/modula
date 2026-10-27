//! Monorepo detection for the "GitHub repo" picker: a JavaScript workspace
//! (pnpm, npm, yarn or bun) becomes one suggested service per app (a package
//! with a `start` or `dev` script; libraries have neither), built from the repository root (where the lockfile is)
//! with the package manager's filter, and deployed only when its own
//! directory changes.
//!
//! Everything is read through the GitHub API (the tree, then a capped number
//! of small blobs); nothing is cloned and nothing here is ever run.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use futures::StreamExt;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::authz::Grant;
use crate::deploy::config::{ServiceConfig, validate_branch, validate_repo};
use crate::error::AppResult;
use crate::github::app::{self, TokenScope, TreeEntry};
use crate::ratelimit::Limit;
use crate::state::AppState;

/// One detection reads up to `MAX_PACKAGES + 2` blobs.
const DETECT_RATE: Limit = Limit::per_minute(10);
const MAX_TREE_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_PACKAGES: usize = 50;
/// Blob reads in flight at once.
const FETCH_CONCURRENCY: usize = 8;

#[derive(Deserialize)]
pub struct DetectQuery {
    /// `owner/name`.
    repo: String,
    branch: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detection {
    /// None when the repository has no root package.json.
    package_manager: Option<PackageManager>,
    /// Empty when it isn't a workspace (add it as one service).
    services: Vec<DetectedService>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedService {
    /// A valid service name, not yet unique within the project.
    name: String,
    package_name: String,
    /// The package's directory, relative to the root (`apps/api`).
    path: String,
    /// A complete repository service config.
    config: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl PackageManager {
    /// `packageManager` in package.json (`pnpm@9.1.0`) wins over lockfiles.
    fn detect(field: Option<&str>, has: impl Fn(&str) -> bool) -> Self {
        let named = field.and_then(|f| match f.split('@').next() {
            Some("npm") => Some(Self::Npm),
            Some("pnpm") => Some(Self::Pnpm),
            Some("yarn") => Some(Self::Yarn),
            Some("bun") => Some(Self::Bun),
            _ => None,
        });
        named.unwrap_or(if has("pnpm-lock.yaml") || has("pnpm-workspace.yaml") {
            Self::Pnpm
        } else if has("yarn.lock") {
            Self::Yarn
        } else if has("bun.lock") || has("bun.lockb") {
            Self::Bun
        } else {
            Self::Npm
        })
    }

    /// Runs `script` of package `pkg` from the repository root, as argv.
    fn run(self, pkg: &str, script: &str) -> Vec<String> {
        let argv: &[&str] = match self {
            Self::Pnpm => &["pnpm", "--filter", pkg, script],
            Self::Npm => &["npm", "run", script, "-w", pkg],
            Self::Yarn => &["yarn", "workspace", pkg, script],
            Self::Bun => &["bun", "run", "--filter", pkg, script],
        };
        argv.iter().map(|s| (*s).to_owned()).collect()
    }
}

/// GET /api/workspaces/{id}/github/detect?repo=owner/name&branch=main
pub async fn detect(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<DetectQuery>,
) -> AppResult<Json<Detection>> {
    validate_repo(&q.repo)?;
    validate_branch(&q.branch)?;
    let workspace = grant.workspace_id()?;
    let nothing = || {
        Json(Detection {
            package_manager: None,
            services: Vec::new(),
        })
    };
    // Only through an installation: anonymous API calls are scarce (60 an
    // hour per server) and deploys need them more.
    let Some(token) = app::repo_token(
        &state,
        workspace,
        &q.repo,
        TokenScope::Contents(q.repo.clone()),
    )
    .await?
    else {
        return Ok(nothing());
    };
    state
        .limiter
        .check("github_detect", &workspace.to_string(), DETECT_RATE)
        .await?;
    let head = app::branch_head(&state, workspace, &q.repo, &q.branch).await?;
    let Some(tree) = app::repo_tree(&state, &token, &q.repo, &head.sha, MAX_TREE_BYTES).await?
    else {
        return Ok(nothing());
    };
    let blobs: HashMap<&str, &TreeEntry> = tree
        .iter()
        .filter(|e| e.kind == "blob")
        .map(|e| (e.path.as_str(), e))
        .collect();
    let read = |path: &str| read_text(&state, &token, &q.repo, blobs.get(path).copied());

    let Some(root) = read("package.json").await? else {
        return Ok(nothing());
    };
    let Ok(root) = serde_json::from_str::<RootPackage>(&root) else {
        return Ok(nothing());
    };
    let pm = PackageManager::detect(root.package_manager.as_deref(), |p| blobs.contains_key(p));
    let patterns = match read("pnpm-workspace.yaml").await? {
        Some(yaml) => pnpm_packages(&yaml),
        None => root
            .workspaces
            .map(Workspaces::patterns)
            .unwrap_or_default(),
    };
    let globs = Globs::new(&patterns);
    let dirs = package_dirs(tree.iter().filter(|e| e.kind == "blob"), &globs);

    // Owned per read: the stream's futures can't borrow across the handler's
    // higher-ranked lifetimes.
    let dirs: Vec<(String, TreeEntry)> = dirs
        .into_iter()
        .map(|(d, e)| (d.to_owned(), e.clone()))
        .collect();
    let reads = dirs.into_iter().map(|(dir, entry)| {
        let (state, token, repo) = (state.clone(), token.clone(), q.repo.clone());
        async move {
            let text = read_text(&state, &token, &repo, Some(&entry)).await?;
            AppResult::Ok(text.map(|t| (dir, t)))
        }
    });
    let packages: Vec<_> = futures::stream::iter(reads)
        .buffered(FETCH_CONCURRENCY)
        .collect()
        .await;
    let mut services = Vec::new();
    for p in packages {
        let Some((dir, text)) = p? else { continue };
        if let Some(mut s) = service_for(pm, &q.repo, &q.branch, &dir, &text) {
            // An app with a Dockerfile of its own builds with it.
            let own = format!("{dir}/Dockerfile");
            if blobs.contains_key(own.as_str()) {
                s.config["builder"] = json!("auto");
                s.config["dockerfilePath"] = json!(own);
            }
            services.push(s);
        }
    }
    Ok(Json(Detection {
        package_manager: Some(pm),
        services,
    }))
}

/// A small UTF-8 file from the tree (None when absent, too big, or binary).
async fn read_text(
    state: &AppState,
    token: &SecretString,
    repo: &str,
    entry: Option<&TreeEntry>,
) -> AppResult<Option<String>> {
    let Some(e) = entry.filter(|e| e.size.unwrap_or(0) <= MAX_FILE_BYTES as u64) else {
        return Ok(None);
    };
    let bytes = app::repo_blob(state, token, repo, &e.sha, MAX_FILE_BYTES).await?;
    Ok(bytes.and_then(|b| String::from_utf8(b).ok()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RootPackage {
    package_manager: Option<String>,
    workspaces: Option<Workspaces>,
}

/// npm/yarn/bun: `"workspaces": [...]`, or yarn's `{ "packages": [...] }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Workspaces {
    List(Vec<String>),
    Object {
        #[serde(default)]
        packages: Vec<String>,
    },
}

impl Workspaces {
    fn patterns(self) -> Vec<String> {
        match self {
            Self::List(p) | Self::Object { packages: p } => p,
        }
    }
}

#[derive(Deserialize)]
struct Package {
    name: Option<String>,
    #[serde(default)]
    scripts: HashMap<String, Value>,
}

/// The suggested service for the package in `dir`, if it is an app: it has a
/// `start` script, or a `dev` one (e.g. a Vite site, which Railpack serves
/// from its build output).
fn service_for(
    pm: PackageManager,
    repo: &str,
    branch: &str,
    dir: &str,
    package_json: &str,
) -> Option<DetectedService> {
    let pkg: Package = serde_json::from_str(package_json).ok()?;
    let name = pkg.name.filter(|n| valid_package_name(n))?;
    let has = |s: &str| pkg.scripts.get(s).is_some_and(Value::is_string);
    if !has("start") && !has("dev") {
        return None;
    }
    // Railpack, explicitly: a Dockerfile at the repository root belongs to
    // one app at most (the caller switches to an app's own Dockerfile).
    let mut config = json!({
        "source": { "type": "repo", "repo": repo, "branch": branch },
        "builder": "railpack",
        "watchPaths": [format!("/{dir}/**")],
    });
    if has("start") {
        config["startCommand"] = json!(pm.run(&name, "start"));
        // Ours, not the user's: a Dockerfile build runs its image's CMD instead.
        config["startCommandDetected"] = json!(true);
    }
    if has("build") {
        // A shell string, but `name` is limited to shell-inert characters.
        config["buildCommand"] = json!(pm.run(&name, "build").join(" "));
    }
    // Whatever the frontend sends must pass the server's own checks.
    let parsed: ServiceConfig = serde_json::from_value(config.clone()).ok()?;
    parsed.validate().ok()?;
    let service = service_name(&name)
        .or_else(|| service_name(dir.rsplit('/').next().unwrap_or(dir)))
        .unwrap_or_else(|| "service".to_owned());
    Some(DetectedService {
        name: service,
        package_name: name,
        path: dir.to_owned(),
        config,
    })
}

/// npm package names, minus anything a shell would interpret (`~`).
fn valid_package_name(n: &str) -> bool {
    (1..=214).contains(&n.len())
        && n.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '@')
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._@/".contains(&b))
}

/// `@anton/api` → `api`, `Admin_UI` → `admin-ui`; None when nothing is left.
fn service_name(package: &str) -> Option<String> {
    let base = package
        .rsplit('/')
        .next()
        .unwrap_or(package)
        .to_ascii_lowercase();
    let mut out = String::new();
    for c in base.chars() {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if c == '-' && (out.is_empty() || out.ends_with('-')) {
            continue;
        }
        out.push(c);
    }
    let out = out.trim_start_matches(|c: char| !c.is_ascii_lowercase());
    let out: String = out.chars().take(63).collect();
    let out = out.trim_end_matches('-');
    (!out.is_empty()).then(|| out.to_owned())
}

/// The `packages:` list of a pnpm-workspace.yaml: block (`- 'apps/*'`) or
/// flow (`['apps/*']`) style; the rest of the file is ignored.
fn pnpm_packages(yaml: &str) -> Vec<String> {
    let unquote = |s: &str| {
        let s = s.trim();
        s.strip_prefix('\'')
            .and_then(|r| r.strip_suffix('\''))
            .or_else(|| s.strip_prefix('"').and_then(|r| r.strip_suffix('"')))
            .unwrap_or(s)
            .to_owned()
    };
    let mut out = Vec::new();
    let mut inside = false;
    for raw in yaml.lines() {
        // Comments: `#` at the start or after whitespace.
        let line = match raw.find(" #") {
            Some(i) => &raw[..i],
            None if raw.trim_start().starts_with('#') => "",
            None => raw,
        };
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        if !indented && !t.starts_with('-') {
            inside = false;
            if let Some(rest) = t.strip_prefix("packages:") {
                let rest = rest.trim();
                if let Some(flow) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    out.extend(flow.split(',').map(unquote).filter(|s| !s.is_empty()));
                } else {
                    inside = rest.is_empty();
                }
            }
            continue;
        }
        if inside && let Some(item) = t.strip_prefix('-') {
            out.push(unquote(item));
        }
    }
    out
}

/// Workspace globs (`apps/*`, `packages/**`, `!**/test/**`) over directories.
struct Globs {
    include: GlobSet,
    exclude: GlobSet,
}

impl Globs {
    fn new(patterns: &[String]) -> Self {
        let mut include = GlobSetBuilder::new();
        let mut exclude = GlobSetBuilder::new();
        for raw in patterns {
            let (negated, p) = match raw.trim().strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw.trim()),
            };
            let p = p.trim_start_matches("./").trim_matches('/');
            if p.is_empty() {
                continue;
            }
            // A pattern that doesn't compile matches nothing.
            let Ok(g) = GlobBuilder::new(p).literal_separator(true).build() else {
                continue;
            };
            if negated {
                exclude.add(g);
            } else {
                include.add(g);
            }
        }
        let build = |b: GlobSetBuilder| b.build().unwrap_or_else(|_| GlobSet::empty());
        Self {
            include: build(include),
            exclude: build(exclude),
        }
    }

    fn matches(&self, dir: &str) -> bool {
        self.include.is_match(dir) && !self.exclude.is_match(dir)
    }
}

/// Directories holding a workspace package.json, in path order, at most
/// [`MAX_PACKAGES`].
fn package_dirs<'a>(
    blobs: impl Iterator<Item = &'a TreeEntry>,
    globs: &Globs,
) -> Vec<(&'a str, &'a TreeEntry)> {
    let mut out: Vec<_> = blobs
        .filter_map(|e| Some((e.path.strip_suffix("/package.json")?, e)))
        .filter(|(dir, _)| !dir.split('/').any(|s| s == "node_modules") && globs.matches(dir))
        .collect();
    out.sort_by_key(|(dir, _)| *dir);
    out.truncate(MAX_PACKAGES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pnpm_workspace_globs_pick_package_dirs() {
        let yaml = "# workspace\npackages:\n  - 'apps/*'\n  - \"packages/**\" # all\n  - '!**/test/**'\n\ncatalog:\n  react: ^19\n";
        let patterns = pnpm_packages(yaml);
        assert_eq!(patterns, ["apps/*", "packages/**", "!**/test/**"]);
        assert_eq!(
            pnpm_packages("packages: ['apps/*', web]"),
            ["apps/*", "web"]
        );

        let entry = |path: &str| TreeEntry {
            path: path.into(),
            kind: "blob".into(),
            sha: "0".repeat(40),
            size: Some(10),
        };
        let tree: Vec<_> = [
            "package.json",
            "apps/api/package.json",
            "apps/admin/package.json",
            "apps/api/src/package.json",
            "apps/api/node_modules/x/package.json",
            "packages/ui/core/package.json",
            "packages/test/fixture/package.json",
            "docs/package.json",
        ]
        .into_iter()
        .map(entry)
        .collect();
        let dirs: Vec<_> = package_dirs(tree.iter(), &Globs::new(&patterns))
            .into_iter()
            .map(|(d, _)| d)
            .collect();
        assert_eq!(dirs, ["apps/admin", "apps/api", "packages/ui/core"]);
    }

    #[test]
    fn services_build_from_the_root_with_a_filter() {
        let pkg =
            r#"{"name":"@anton/template-studio","scripts":{"build":"tsc","start":"node dist"}}"#;
        let s = service_for(
            PackageManager::Pnpm,
            "acme/mono",
            "main",
            "apps/template-studio",
            pkg,
        )
        .unwrap();
        assert_eq!(s.name, "template-studio");
        assert_eq!(
            s.config["buildCommand"],
            "pnpm --filter @anton/template-studio build"
        );
        assert_eq!(
            s.config["startCommand"],
            json!(["pnpm", "--filter", "@anton/template-studio", "start"])
        );
        assert_eq!(s.config["watchPaths"], json!(["/apps/template-studio/**"]));
        assert!(s.config["source"].get("rootDir").is_none());
        // A site with only dev and build scripts: built, no start command.
        let site = service_for(
            PackageManager::Pnpm,
            "a/b",
            "main",
            "apps/site",
            r#"{"name":"@a/site","scripts":{"dev":"vite","build":"vite build"}}"#,
        )
        .unwrap();
        assert!(site.config.get("startCommand").is_none());
        assert_eq!(site.config["buildCommand"], "pnpm --filter @a/site build");
        // A library (build only), or a name a shell would expand: not offered.
        assert!(
            service_for(
                PackageManager::Npm,
                "a/b",
                "main",
                "x",
                r#"{"name":"x","scripts":{"build":"tsc"}}"#
            )
            .is_none()
        );
        assert!(
            service_for(
                PackageManager::Npm,
                "a/b",
                "main",
                "x",
                r#"{"name":"$(x)","scripts":{"start":"x"}}"#
            )
            .is_none()
        );
        assert_eq!(
            service_name("@scope/9Admin_UI").as_deref(),
            Some("admin-ui")
        );
    }
}
