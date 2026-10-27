//! `modula init`: set up a directory, like `railway init`. Pick (or create)
//! the project, link the directory to it and an environment, then write the
//! project's config to `.modula/` (TypeScript unless `--format toml`). In a
//! terminal each choice is a list; without one, flags decide.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::bail;
use inquire::{Confirm, InquireError, MultiSelect, Text};
use serde::Serialize;

use crate::api::{Environment, Me, ProjectDetail, Workspace};
use crate::cli::ConfigFormat;
use crate::commands::config::{Written, describe_written, write_config};
use crate::config::{LINK_DIR, Link};
use crate::context::Ctx;
use crate::iac::model::FILES;
use crate::output::usage;
use crate::resolve;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Initialized {
    /// The project's id.
    id: String,
    name: String,
    workspace: String,
    environment: String,
    /// Whether the project was created now.
    created: bool,
    link: PathBuf,
    /// The config file written, if any.
    config: Option<Written>,
}

/// A prompt's answer; Esc and Ctrl-C cancel.
fn answer<T>(r: Result<T, InquireError>) -> anyhow::Result<T> {
    match r {
        Ok(v) => Ok(v),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
            bail!("cancelled")
        }
        Err(e) => Err(e.into()),
    }
}

/// One of `items`, picked from a list (type to filter).
fn choose<T: Clone>(prompt: &str, items: &[T], label: impl Fn(&T) -> String) -> anyhow::Result<T> {
    resolve::choose(prompt, items, label).cloned()
}

/// With several saved logins and none named (`--login`, `MODULA_TOKEN`, a
/// directory token), which instance to set up the project on.
fn pick_login(ctx: &mut Ctx) -> anyhow::Result<()> {
    if ctx.global.login.is_some()
        || ctx.session.env_token.is_some()
        || ctx.local_login.is_some()
        || ctx.cfg.logins.len() < 2
    {
        return Ok(());
    }
    let names: Vec<&String> = ctx.cfg.logins.keys().collect();
    let current = ctx.cfg.current.clone();
    let name = choose("Modula server", &names, |n| {
        let url = &ctx.cfg.logins[n.as_str()].url;
        if current.as_deref() == Some(n.as_str()) {
            format!("{n} ({url}, current)")
        } else {
            format!("{n} ({url})")
        }
    })?;
    ctx.global.login = Some(name.clone());
    Ok(())
}

pub async fn run(
    ctx: &mut Ctx,
    name: Option<String>,
    workspace: Option<String>,
    format: Option<ConfigFormat>,
) -> anyhow::Result<()> {
    let interactive = ctx.session.interactive && !ctx.out.json;
    if interactive {
        inquire::set_global_render_config(crate::provision::wizard::theme());
    }
    if !relink_ok(ctx, interactive)? {
        return Ok(());
    }
    if interactive {
        pick_login(ctx)?;
    }
    let api = ctx.api()?;
    let me = api.me().await?;

    // The project: -p picks one, a name creates one, else ask.
    let (project, ws, created) = if let Some(wanted) = ctx.global.project.clone() {
        if name.is_some() {
            return Err(usage(
                "pass a new project's name or -p for an existing one, not both",
            ));
        }
        let p = resolve::project(api, Some(&wanted)).await?;
        let ws = workspace_of(ctx, &me, &p.id).await?;
        (p, ws, false)
    } else {
        let ws = pick_workspace(&me, workspace.as_deref(), interactive)?;
        let picked = match name {
            Some(n) => Picked::New(n),
            None if interactive => pick_project(ctx, &ws).await?,
            None => {
                let existing = api.projects(&ws.id).await?;
                return Err(usage(format!(
                    "pass a name for a new project, or -p for an existing one (projects in {}: {})",
                    ws.name,
                    if existing.is_empty() {
                        "none yet".to_owned()
                    } else {
                        existing
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )));
            }
        };
        match picked {
            Picked::New(name) => (api.create_project(&ws.id, &name).await?, ws, true),
            Picked::Existing(id) => (api.project(&id).await?, ws, false),
        }
    };

    let env = pick_environment(ctx, &project, interactive)?;
    // Link here, not in a parent's linked directory.
    ctx.link = None;
    let link = ctx.save_link(Link::new(&project.id, &project.name, &env))?;

    let config = match config_format(ctx, &project, format, interactive)? {
        Some(format) => {
            let only = partial_services(ctx, &project, &env, interactive)?;
            let path = ctx.session.cwd.join(LINK_DIR).join(format.file_name());
            Some(write_config(ctx, &project, &path, format, only.as_ref()).await?)
        }
        None => None,
    };

    let result = Initialized {
        id: project.id.clone(),
        name: project.name.clone(),
        workspace: ws.name.clone(),
        environment: env.name.clone(),
        created,
        link,
        config,
    };
    ctx.out.either(&result, |o| {
        if result.created {
            o.line(format!(
                "Created {} in {} and linked it ({})",
                result.name, result.workspace, result.environment
            ));
        } else {
            o.line(format!(
                "Linked to {} ({})",
                result.name, result.environment
            ));
        }
        o.note(format!("Wrote {}", result.link.display()));
        if let Some(w) = &result.config {
            o.line(format!(
                "Wrote {}{}",
                w.file.display(),
                if w.partial {
                    " (this repository's services only)"
                } else {
                    ""
                }
            ));
            describe_written(o, w);
        }
    })
}

/// The project chosen in a terminal.
enum Picked {
    /// A new project's name.
    New(String),
    /// An existing project's id.
    Existing(String),
}

/// An existing link in this very directory: ask (or need --yes) before
/// replacing it. Returns false when the user keeps it.
fn relink_ok(ctx: &Ctx, interactive: bool) -> anyhow::Result<bool> {
    let Some((file, link)) = &ctx.link else {
        return Ok(true);
    };
    let here = ctx.session.cwd.join(LINK_DIR);
    if file.parent() != Some(here.as_path()) {
        return Ok(true);
    }
    let what = format!(
        "This directory is already linked to {} ({})",
        link.project_name.as_deref().unwrap_or(&link.project),
        link.environment_name
            .as_deref()
            .unwrap_or("default environment")
    );
    if ctx.global.yes {
        return Ok(true);
    }
    if !interactive {
        return Err(usage(format!(
            "{what}; pass --yes to set it up again, or `modula unlink` first"
        )));
    }
    let again = answer(
        Confirm::new(&format!("{what}. Set it up again?"))
            .with_default(false)
            .prompt(),
    )?;
    if !again {
        ctx.out.line("Kept the link.");
    }
    Ok(again)
}

fn pick_workspace(me: &Me, wanted: Option<&str>, interactive: bool) -> anyhow::Result<Workspace> {
    if wanted.is_some() || me.workspaces.len() <= 1 || !interactive {
        return resolve::pick_workspace(&me.workspaces, wanted).cloned();
    }
    choose("Workspace", &me.workspaces, |w| w.name.clone())
}

/// The workspace holding project `id`.
async fn workspace_of(ctx: &Ctx, me: &Me, id: &str) -> anyhow::Result<Workspace> {
    let api = ctx.api()?;
    for ws in &me.workspaces {
        if api.projects(&ws.id).await?.iter().any(|p| p.id == id) {
            return Ok(ws.clone());
        }
    }
    bail!("project {id} isn't in any of your workspaces")
}

/// "Create a new project" (then its name, defaulting to the repository's
/// directory) or one of the workspace's projects.
async fn pick_project(ctx: &Ctx, ws: &Workspace) -> anyhow::Result<Picked> {
    const NEW: &str = "Create a new project";
    let api = ctx.api()?;
    let existing = api.projects(&ws.id).await?;
    let mut items = vec![NEW.to_owned()];
    items.extend(existing.iter().map(|p| p.name.clone()));
    let picked = if existing.is_empty() {
        NEW.to_owned()
    } else {
        choose("Project", &items, Clone::clone)?
    };
    if picked == NEW {
        let default = default_name(&ctx.session.cwd);
        let name = answer(
            Text::new("Project name")
                .with_default(&default)
                .with_validator(|s: &str| {
                    Ok(if s.trim().is_empty() {
                        inquire::validator::Validation::Invalid("a name, please".into())
                    } else {
                        inquire::validator::Validation::Valid
                    })
                })
                .prompt(),
        )?;
        return Ok(Picked::New(name.trim().to_owned()));
    }
    let p = existing
        .iter()
        .find(|p| p.name == picked)
        .ok_or_else(|| anyhow::anyhow!("no project named {picked}"))?;
    Ok(Picked::Existing(p.id.clone()))
}

/// The repository's directory name (else the current directory's).
fn default_name(cwd: &Path) -> String {
    let root = cwd
        .ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(cwd);
    root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app")
        .to_owned()
}

fn pick_environment(
    ctx: &Ctx,
    p: &ProjectDetail,
    interactive: bool,
) -> anyhow::Result<Environment> {
    let wanted = ctx.global.environment.as_deref();
    if wanted.is_some() || p.environments.len() <= 1 || !interactive {
        return resolve::pick_environment(&p.environments, wanted).cloned();
    }
    let mut envs = p.environments.clone();
    envs.sort_by_key(|e| !e.is_default);
    choose("Environment", &envs, |e| e.name.clone())
}

/// The config to write: --format, else (in a terminal) ask; never over an
/// existing config file.
fn config_format(
    ctx: &Ctx,
    p: &ProjectDetail,
    format: Option<ConfigFormat>,
    interactive: bool,
) -> anyhow::Result<Option<ConfigFormat>> {
    let dir = ctx.session.cwd.join(LINK_DIR);
    if let Some(existing) = FILES.iter().map(|f| dir.join(f)).find(|f| f.is_file()) {
        if format.is_some() || interactive {
            ctx.out.note(format!(
                "Kept {} (`modula config init --force` rewrites it from the project)",
                existing.display()
            ));
        }
        return Ok(None);
    }
    if format.is_some() || !interactive {
        return Ok(format);
    }
    let write = answer(
        Confirm::new(&format!(
            "Write .modula/modula.ts from {}'s current setup?",
            p.name
        ))
        .with_default(true)
        .with_help_message("config as code; `modula init --format toml` writes TOML instead")
        .prompt(),
    )?;
    Ok(write.then_some(ConfigFormat::Ts))
}

/// `owner/name` of the repository's GitHub `origin`, lowercase.
fn origin_repo(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    github_repo(String::from_utf8_lossy(&out.stdout).trim())
}

fn github_repo(url: &str) -> Option<String> {
    let rest = [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
    ]
    .iter()
    .find_map(|p| url.strip_prefix(p))?;
    let repo = rest.trim_end_matches('/').trim_end_matches(".git");
    (repo.split('/').count() == 2).then(|| repo.to_ascii_lowercase())
}

/// The services this repository describes, when it is one of several
/// (`partial`): those deployed from its GitHub repository; in a terminal,
/// chosen from a list. None: all of them.
fn partial_services(
    ctx: &Ctx,
    p: &ProjectDetail,
    env: &Environment,
    interactive: bool,
) -> anyhow::Result<Option<BTreeSet<String>>> {
    let repo_of = |service: &str| {
        p.instances
            .iter()
            .find(|i| i.service_id == service && i.environment_id == env.id)
            .and_then(|i| i.repo.as_deref())
            .map(str::to_ascii_lowercase)
    };
    let origin = origin_repo(&ctx.session.cwd);
    let matching: BTreeSet<String> = p
        .services
        .iter()
        .filter(|s| origin.is_some() && repo_of(&s.id) == origin)
        .map(|s| s.name.clone())
        .collect();
    if p.services.len() <= 1 {
        return Ok(None);
    }
    let chosen: BTreeSet<String> = if interactive {
        let names: Vec<String> = p.services.iter().map(|s| s.name.clone()).collect();
        let defaults: Vec<usize> = names
            .iter()
            .enumerate()
            .filter(|(_, n)| matching.is_empty() || matching.contains(*n))
            .map(|(i, _)| i)
            .collect();
        answer(
            MultiSelect::new("Services this repository describes", names)
                .with_default(&defaults)
                .with_help_message("the others are left alone (partial)")
                .prompt(),
        )?
        .into_iter()
        .collect()
    } else {
        matching
    };
    Ok((!chosen.is_empty() && chosen.len() < p.services.len()).then_some(chosen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes() {
        for url in [
            "git@github.com:Acme/Web.git",
            "https://github.com/acme/web",
            "https://github.com/acme/web.git",
            "ssh://git@github.com/acme/web.git",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("acme/web"), "{url}");
        }
        assert_eq!(github_repo("https://gitlab.com/acme/web"), None);
        assert_eq!(github_repo(""), None);
    }
}
