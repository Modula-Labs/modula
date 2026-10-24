//! `environment`: list, switch the linked one, create, rename or delete one.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use serde::Serialize;

use crate::api::{ApiError, Environment, NewEnvironment, ProjectDetail};
use crate::cli::{EnvironmentArgs, EnvironmentCommand};
use crate::config::Link;
use crate::context::Ctx;
use crate::output::{Table, usage};
use crate::resolve;

#[derive(Debug, Serialize)]
struct EnvironmentRow {
    id: String,
    name: String,
    default: bool,
    selected: bool,
}

/// Point the link at `env` of project `p`, keeping its service.
fn relink(ctx: &mut Ctx, p: &ProjectDetail, env: &Environment) -> anyhow::Result<Link> {
    let mut link = match ctx.linked() {
        Some(l) if l.project == p.id => l.clone(),
        _ => Link::new(&p.id, &p.name, env),
    };
    link.environment = Some(env.id.clone());
    link.environment_name = Some(env.name.clone());
    ctx.save_link(link.clone())?;
    Ok(link)
}

/// `SERVICE=BRANCH` flags to service id → branch.
fn branches(p: &ProjectDetail, flags: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for f in flags {
        let Some((service, branch)) = f.split_once('=') else {
            return Err(usage(format!("--branch {f}: expected SERVICE=BRANCH")));
        };
        let s = resolve::pick_service(&p.services, Some(service.trim()))?;
        out.insert(s.id.clone(), branch.trim().to_owned());
    }
    Ok(out)
}

pub async fn run(ctx: &mut Ctx, args: EnvironmentArgs) -> anyhow::Result<()> {
    let p = ctx.project().await?;
    match (args.command, args.name) {
        (
            Some(EnvironmentCommand::New {
                name,
                duplicate,
                branches: flags,
            }),
            _,
        ) => {
            let duplicate_from = duplicate
                .as_deref()
                .map(|d| resolve::pick_environment(&p.environments, Some(d)).map(|e| e.id.clone()))
                .transpose()?;
            let req = NewEnvironment {
                name,
                duplicate_from,
                branches: branches(&p, &flags)?,
            };
            let env = ctx.api()?.create_environment(&p.id, &req).await?;
            relink(ctx, &p, &env)?;
            ctx.out.either(&env, |o| {
                o.line(format!("Created {} in {} and linked it", env.name, p.name));
                o.note("Its services aren't deployed yet: run `modula deploy` there.");
            })
        }
        (Some(EnvironmentCommand::Rename { environment, name }), _) => {
            let old = resolve::pick_environment(&p.environments, Some(&environment))?.clone();
            let env = ctx.api()?.rename_environment(&old.id, &name).await?;
            // Keep the human-readable name in a link to it current.
            if let Some(l) = ctx.linked()
                && l.environment.as_deref() == Some(env.id.as_str())
            {
                relink(ctx, &p, &env)?;
            }
            ctx.out.either(&env, |o| {
                o.line(format!("Renamed {} to {}", old.name, env.name));
            })
        }
        (Some(EnvironmentCommand::Delete { environment }), _) => {
            let env = resolve::pick_environment(&p.environments, Some(&environment))?.clone();
            if env.is_default {
                bail!(
                    "{} is the default environment: it can't be deleted",
                    env.name
                );
            }
            ctx.confirm(&format!(
                "Delete {} from {}, with its deployments, variables and volume data?",
                env.name, p.name
            ))?;
            let api = ctx.api()?;
            if let Err(e) = api.delete_environment(&env.id).await {
                let sudo = e
                    .downcast_ref::<ApiError>()
                    .is_some_and(|a| a.code == "sudo_required");
                if sudo {
                    let url = api.login.url.join(&format!(
                        "projects/{}/settings/environments?env={}",
                        p.id, env.id
                    ))?;
                    return Err(e).context(format!("delete it in the dashboard: {url}"));
                }
                return Err(e);
            }
            let unlinked = ctx
                .linked()
                .is_some_and(|l| l.environment.as_deref() == Some(env.id.as_str()));
            ctx.out.either(&serde_json::json!({ "deleted": env.id }), |o| {
                o.line(format!("Deleted {} from {}", env.name, p.name));
                if unlinked {
                    o.note("This directory was linked to it: run `modula environment <name>` to pick another.");
                }
            })
        }
        (None, Some(name)) => {
            let env = resolve::pick_environment(&p.environments, Some(&name))?.clone();
            let link = relink(ctx, &p, &env)?;
            ctx.out.either(&link, |o| {
                o.line(format!("Linked {} ({})", p.name, env.name));
            })
        }
        (None, None) => {
            let current = ctx.environment(&p).ok().map(|e| e.id.clone());
            let rows: Vec<EnvironmentRow> = p
                .environments
                .iter()
                .map(|e| EnvironmentRow {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    default: e.is_default,
                    selected: current.as_deref() == Some(e.id.as_str()),
                })
                .collect();
            ctx.out.either(&rows, |o| {
                let mut t = Table::new(&["environment", "id", ""]);
                for r in &rows {
                    let mark = match (r.selected, r.default) {
                        (true, true) => "(selected, default)",
                        (true, false) => "(selected)",
                        (false, true) => "(default)",
                        (false, false) => "",
                    };
                    t.row([r.name.as_str(), &r.id, mark]);
                }
                o.table(&t);
            })
        }
    }
}
