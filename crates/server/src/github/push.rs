//! Webhook deliveries: signature verification and push-to-deploy.

use globset::{GlobBuilder, GlobMatcher};
use hmac::{Hmac, KeyInit, Mac};
use modula_id::Id;
use serde::Deserialize;
use sha2::Sha256;

use super::Commit;
use crate::audit::Entry;
use crate::deploy::config::ServiceConfig;
use crate::deploy::start::{NewDeployment, Trigger, start};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

/// `X-Hub-Signature-256: sha256=<hex>` over the raw body, in constant time.
pub fn verify_signature(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(sig) = header
        .and_then(|h| h.strip_prefix("sha256="))
        .and_then(|h| hex::decode(h).ok())
    else {
        return false;
    };
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).expect("any key length");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

#[derive(Deserialize)]
pub struct PushEvent {
    #[serde(rename = "ref")]
    git_ref: String,
    after: String,
    #[serde(default)]
    deleted: bool,
    repository: Repository,
    installation: Option<InstallationRef>,
    head_commit: Option<HeadCommit>,
    #[serde(default)]
    commits: Vec<PushCommit>,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

#[derive(Deserialize)]
pub struct InstallationRef {
    pub id: i64,
}

#[derive(Deserialize)]
struct HeadCommit {
    message: String,
    author: Option<Author>,
}

#[derive(Deserialize)]
struct Author {
    name: String,
    username: Option<String>,
}

#[derive(Deserialize)]
struct PushCommit {
    #[serde(default)]
    added: Vec<String>,
    #[serde(default)]
    modified: Vec<String>,
    #[serde(default)]
    removed: Vec<String>,
}

impl PushEvent {
    /// Every path the push touched; `None` when GitHub sent no file lists
    /// (e.g. a push of only already-known commits), so filters can't apply.
    fn changed_paths(&self) -> Option<Vec<&str>> {
        if self.commits.is_empty() {
            return None;
        }
        Some(
            self.commits
                .iter()
                .flat_map(|c| c.added.iter().chain(&c.modified).chain(&c.removed))
                .map(String::as_str)
                .collect(),
        )
    }
}

/// gitignore-style watch paths: a pattern without an inner `/` matches at any
/// depth, a leading `/` anchors it at the repository root, a trailing `/` (or
/// a directory match) covers everything below, `!` re-excludes. The last
/// matching pattern decides, as in `.gitignore`.
pub struct WatchPaths(Vec<(GlobMatcher, bool)>);

impl WatchPaths {
    pub fn new(patterns: &[String]) -> Option<Self> {
        let mut out = Vec::new();
        for raw in patterns {
            let (negated, p) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw.as_str()),
            };
            let trimmed = p.trim_end_matches('/');
            let anchored = trimmed.starts_with('/') || trimmed.contains('/');
            let base = trimmed.trim_start_matches('/');
            if base.is_empty() {
                continue;
            }
            let base = if anchored || base.starts_with("**") {
                base.to_owned()
            } else {
                format!("**/{base}")
            };
            for glob in [base.clone(), format!("{base}/**")] {
                // A pattern that doesn't compile can't match anything.
                let m = GlobBuilder::new(&glob)
                    .literal_separator(true)
                    .build()
                    .ok()?;
                out.push((m.compile_matcher(), negated));
            }
        }
        Some(Self(out))
    }

    pub fn matches(&self, path: &str) -> bool {
        let mut included = false;
        for (m, negated) in &self.0 {
            if m.is_match(path) {
                included = !negated;
            }
        }
        included
    }
}

/// Whether a push with `changed` paths should deploy a service watching `patterns`.
pub fn should_deploy(patterns: &[String], changed: Option<&[&str]>) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let Some(changed) = changed else {
        return true;
    };
    match WatchPaths::new(patterns) {
        Some(w) => changed.iter().any(|p| w.matches(p)),
        None => false,
    }
}

pub fn is_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Start a `push` deployment for every service instance that tracks the
/// pushed branch with auto-deploy on, in a workspace that installed the App
/// for this repository's owner. Pull request previews deploy on
/// `pull_request` events instead (see `pull_request`). Returns the new deployment ids. Per-instance
/// failures (e.g. no server available) are logged and skipped.
pub async fn handle_push(
    state: &AppState,
    meta: &RequestMeta,
    app: i64,
    delivery: &str,
    ev: &PushEvent,
) -> AppResult<Vec<Id>> {
    let Some(branch) = ev.git_ref.strip_prefix("refs/heads/") else {
        return Ok(vec![]); // tags
    };
    let (Some(installation), Some(head)) = (&ev.installation, &ev.head_commit) else {
        return Ok(vec![]);
    };
    if ev.deleted || !is_sha(&ev.after) {
        return Ok(vec![]);
    }
    let repo = &ev.repository.full_name;
    let owner = repo.split_once('/').map_or(repo.as_str(), |(o, _)| o);
    let candidates: Vec<(Id, Id, Id, serde_json::Value)> = sqlx::query_as(
        "SELECT i.id, i.environment_id, p.workspace_id, i.config
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         JOIN environments e ON e.id = i.environment_id
         JOIN github_installations g ON g.workspace_id = p.workspace_id
         WHERE g.id = $1 AND g.app_id = $5 AND lower(g.account_login) = lower($2)
           AND NOT e.ephemeral
           AND i.config->'source'->>'type' = 'repo'
           AND lower(i.config->'source'->>'repo') = lower($3)
           AND i.config->'source'->>'branch' = $4
           AND coalesce((i.config->>'autoDeploy')::boolean, true)
         ORDER BY i.id",
    )
    .bind(installation.id)
    .bind(owner)
    .bind(repo)
    .bind(branch)
    .bind(app)
    .fetch_all(&state.db)
    .await?;
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    let commit = Commit {
        sha: ev.after.clone(),
        message: head
            .message
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(1000)
            .collect(),
        author: head
            .author
            .as_ref()
            .map(|a| a.name.chars().take(200).collect())
            .unwrap_or_default(),
        author_login: head
            .author
            .as_ref()
            .and_then(|a| a.username.clone())
            .filter(|l| super::valid_login(l)),
    };
    let changed = ev.changed_paths();
    let mut started = Vec::new();
    for (instance, env_id, workspace, config) in candidates {
        let cfg: ServiceConfig = match serde_json::from_value(config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%instance, error = %e, "unreadable service config; push ignored");
                continue;
            }
        };
        if !cfg.auto_deploy || !should_deploy(&cfg.watch_paths, changed.as_deref()) {
            continue;
        }
        if let Some(id) = deploy(
            state,
            meta,
            delivery,
            instance,
            env_id,
            workspace,
            cfg,
            Some(&commit),
        )
        .await?
        {
            started.push(id);
        }
    }
    Ok(started)
}

/// Start a `push` deployment of `commit` on `instance` (None: an image or
/// template service), unless the instance already has one of that commit:
/// delivery ids aren't signed, commits are, so a replayed (or re-sent) event
/// starts nothing. Failures to start (e.g. no server available) are logged
/// and give None.
#[allow(clippy::too_many_arguments)]
pub async fn deploy(
    state: &AppState,
    meta: &RequestMeta,
    delivery: &str,
    instance: Id,
    env_id: Id,
    workspace: Id,
    cfg: ServiceConfig,
    commit: Option<&Commit>,
) -> AppResult<Option<Id>> {
    if let Some(commit) = commit {
        let seen: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM deployments
                            WHERE service_instance_id = $1 AND commit_sha = $2)",
        )
        .bind(instance)
        .bind(&commit.sha)
        .fetch_one(&state.db)
        .await?;
        if seen {
            tracing::info!(%instance, commit = %commit.sha, delivery, "commit already deployed; ignored");
            return Ok(None);
        }
    }
    let source = cfg.source_label();
    let mut tx = state.db.begin().await?;
    let new = NewDeployment {
        instance,
        config: cfg,
        trigger: Trigger::Push,
        created_by: None,
        commit: commit.cloned(),
        local_git: None,
        prebuilt: None,
        source_deployment: None,
    };
    match start(state, &mut tx, env_id, new).await {
        Ok(s) => {
            Entry::new("deployment.create", meta)
                .workspace(workspace)
                .target("deployment", s.id)
                .metadata(serde_json::json!({
                    "serviceInstance": instance,
                    "source": source,
                    "commit": commit.map(|c| &c.sha),
                    "node": s.node,
                    "trigger": "push",
                    "delivery": delivery,
                }))
                .record(&mut *tx)
                .await?;
            tx.commit().await?;
            state.hub.kick_all(&s.kick);
            Ok(Some(s.id))
        }
        Err(AppError::Internal(e)) => Err(AppError::Internal(e)),
        Err(e) => {
            tracing::warn!(%instance, error = %e, "push deployment not started");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn watch_paths_are_gitignore_style() {
        let api = w(&["services/api/", "!services/api/**/*.md"]);
        assert!(should_deploy(&api, Some(&["services/api/src/main.rs"])));
        assert!(!should_deploy(&api, Some(&["services/api/README.md"])));
        assert!(!should_deploy(&api, Some(&["services/web/index.ts"])));
        // Unanchored names match at any depth; anchored ones only at the root.
        assert!(should_deploy(&w(&["*.go"]), Some(&["cmd/x/main.go"])));
        assert!(should_deploy(&w(&["/go.mod"]), Some(&["go.mod"])));
        assert!(!should_deploy(&w(&["/go.mod"]), Some(&["vendor/go.mod"])));
        assert!(!should_deploy(&w(&["src/*.rs"]), Some(&["src/a/b.rs"])));
        assert!(should_deploy(&w(&["src/**/*.rs"]), Some(&["src/a/b.rs"])));
        // No filter, or no file lists from GitHub: always deploy.
        assert!(should_deploy(&[], Some(&["anything"])));
        assert!(should_deploy(&api, None));
    }

    #[test]
    fn signatures() {
        let body = br#"{"zen":"hi"}"#;
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(b"s3cret").unwrap();
        mac.update(body);
        let good = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_signature(b"s3cret", body, Some(&good)));
        assert!(!verify_signature(b"other", body, Some(&good)));
        assert!(!verify_signature(b"s3cret", b"{}", Some(&good)));
        assert!(!verify_signature(b"s3cret", body, None));
        assert!(!verify_signature(b"s3cret", body, Some("sha1=abc")));
    }
}
