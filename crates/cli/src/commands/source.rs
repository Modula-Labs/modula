//! `source`: show or change where a service's code comes from: connect a
//! GitHub repository (or switch to another one), a prebuilt image, or local
//! files deployed with `modula up` (which disconnects the repository).
//! Like other settings, the change is pending until the next deploy.

use serde_json::{Value, json};

use crate::cli::SourceArgs;
use crate::context::Ctx;
use crate::output::usage;

/// `acme/api@main (root web)`, `nginx:1.27`, `local files (modula up)`.
pub fn describe(source: &Value) -> String {
    match source["type"].as_str() {
        Some("repo") => {
            let mut s = format!(
                "{}@{}",
                source["repo"].as_str().unwrap_or("?"),
                source["branch"].as_str().unwrap_or("main")
            );
            if let Some(d) = source["rootDir"].as_str() {
                s.push_str(&format!(" (root {d})"));
            }
            s
        }
        Some("image") => source["image"].as_str().unwrap_or("?").to_owned(),
        Some("local") => "local files (modula up)".to_owned(),
        _ => "-".to_owned(),
    }
}

/// The new source from the flags (None: just show the current one). A repo
/// keeps the current branch and root directory unless given.
pub fn new_source(args: &SourceArgs, current: &Value) -> anyhow::Result<Option<Value>> {
    let root = |cur: &Value| {
        args.root_dir
            .clone()
            .map(Value::from)
            .or_else(|| cur.get("rootDir").cloned())
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
    };
    let mut s = if let Some(repo) = &args.repo {
        let repo = repo
            .trim()
            .trim_start_matches("https://github.com/")
            .trim_end_matches(".git")
            .trim_end_matches('/');
        if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
            return Err(usage("--repo must be owner/name"));
        }
        let branch = args
            .branch
            .clone()
            .or_else(|| {
                (current["type"] == "repo")
                    .then(|| current["branch"].as_str().map(str::to_owned))
                    .flatten()
            })
            .unwrap_or_else(|| "main".into());
        json!({ "type": "repo", "repo": repo, "branch": branch })
    } else if let Some(image) = &args.image {
        json!({ "type": "image", "image": image.trim() })
    } else if args.local {
        json!({ "type": "local" })
    } else if args.branch.is_some() || args.root_dir.is_some() {
        if current["type"] == "image" {
            return Err(usage("an image has no branch or root directory"));
        }
        let mut s = current.clone();
        if let Some(b) = &args.branch {
            if s["type"] != "repo" {
                return Err(usage("--branch needs a repository: pass --repo owner/name"));
            }
            s["branch"] = Value::from(b.clone());
        }
        s
    } else {
        return Ok(None);
    };
    if s["type"] != "image" {
        match root(current) {
            Some(r) => s["rootDir"] = r,
            None => {
                if let Some(o) = s.as_object_mut() {
                    o.remove("rootDir");
                }
            }
        }
    }
    Ok(Some(s))
}

pub async fn run(ctx: &Ctx, args: SourceArgs) -> anyhow::Result<()> {
    let t = ctx.target().await?;
    let api = ctx.api()?;
    let detail = api.instance(&t.instance).await?;
    if detail.template.is_some() {
        return Err(usage(format!(
            "{} is a database from a template; its source can't change",
            t.service.name
        )));
    }
    let current = detail.config["source"].clone();
    let Some(next) = new_source(&args, &current)? else {
        return ctx.out.either(&current, |o| {
            o.line(format!("{}: {}", t.label(), describe(&current)));
        });
    };
    if next == current {
        return ctx.out.either(&current, |o| {
            o.line(format!("{} already uses {}", t.label(), describe(&current)));
        });
    }
    let mut config = detail.config.clone();
    config["source"] = next.clone();
    // Auto-deploy and watch paths only mean something for a repository.
    if next["type"] != "repo"
        && let Some(o) = config.as_object_mut()
    {
        o.remove("autoDeploy");
        o.remove("watchPaths");
    }
    let _: Value = api
        .patch(
            &format!("/api/service-instances/{}", t.instance),
            &json!({ "config": config }),
        )
        .await?;
    ctx.out.either(&next, |o| {
        o.line(format!(
            "{}: {} → {}",
            t.label(),
            describe(&current),
            describe(&next)
        ));
        o.note(match next["type"].as_str() {
            Some("local") => "Deploy it with `modula up` from its directory; pushes no longer deploy.",
            Some("repo") => "Deploy it with `modula deploy` (pending until then); new commits deploy on their own.",
            _ => "Deploy it with `modula deploy` (pending until then).",
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(repo: Option<&str>, branch: Option<&str>, local: bool) -> SourceArgs {
        SourceArgs {
            repo: repo.map(Into::into),
            branch: branch.map(Into::into),
            root_dir: None,
            image: None,
            local,
        }
    }

    #[test]
    fn switches() {
        let local = json!({ "type": "local", "rootDir": "apps/api" });
        // Connect a repo: keeps the root directory, defaults to main.
        let s = new_source(
            &args(Some("https://github.com/Acme/api.git"), None, false),
            &local,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            s,
            json!({ "type": "repo", "repo": "Acme/api", "branch": "main", "rootDir": "apps/api" })
        );
        // Change the repo: keeps the branch.
        let repo = json!({ "type": "repo", "repo": "a/b", "branch": "staging" });
        let s = new_source(&args(Some("a/c"), None, false), &repo)
            .unwrap()
            .unwrap();
        assert_eq!(
            s,
            json!({ "type": "repo", "repo": "a/c", "branch": "staging" })
        );
        // Only the branch.
        let s = new_source(&args(None, Some("main"), false), &repo)
            .unwrap()
            .unwrap();
        assert_eq!(s["branch"], "main");
        // Disconnect.
        let s = new_source(&args(None, None, true), &repo).unwrap().unwrap();
        assert_eq!(s, json!({ "type": "local" }));
        // Nothing asked: show.
        assert!(
            new_source(&args(None, None, false), &repo)
                .unwrap()
                .is_none()
        );
        assert!(new_source(&args(Some("nope"), None, false), &repo).is_err());
        assert!(new_source(&args(None, Some("x"), false), &local).is_err());
        assert_eq!(describe(&repo), "a/b@staging");
    }
}
