//! `environment`: list, switch the linked one, or create one.

use serde::Serialize;

use crate::api::NewEnvironment;
use crate::cli::{EnvironmentArgs, EnvironmentCommand};
use crate::config::Link;
use crate::context::Ctx;
use crate::output::Table;
use crate::resolve;

#[derive(Debug, Serialize)]
struct EnvironmentRow {
    id: String,
    name: String,
    selected: bool,
}

/// Point the link at `env` of project `p`, keeping its service.
fn relink(ctx: &mut Ctx, project_id: &str, project_name: &str, env: &str) -> anyhow::Result<Link> {
    let mut link = match ctx.linked() {
        Some(l) if l.project == project_id => l.clone(),
        _ => Link::new(project_id, project_name, env),
    };
    link.environment = Some(env.to_owned());
    ctx.save_link(link.clone())?;
    Ok(link)
}

pub async fn run(ctx: &mut Ctx, args: EnvironmentArgs) -> anyhow::Result<()> {
    let p = ctx.project().await?;
    match (args.command, args.name) {
        (
            Some(EnvironmentCommand::New {
                name,
                copy,
                copy_variables,
            }),
            _,
        ) => {
            let clone_from = copy
                .as_deref()
                .map(|c| resolve::pick_environment(&p.environments, Some(c)).map(|e| e.id.clone()))
                .transpose()?;
            let env = ctx
                .api()?
                .create_environment(
                    &p.id,
                    &NewEnvironment {
                        name,
                        clone_from,
                        copy_variables,
                    },
                )
                .await?;
            relink(ctx, &p.id, &p.name, &env.name)?;
            ctx.out.either(&env, |o| {
                o.line(format!("Created {} in {} and linked it", env.name, p.name));
                o.note("Its services aren't deployed yet: run `modula-deploy deploy` there.");
            })
        }
        (None, Some(name)) => {
            let env = resolve::pick_environment(&p.environments, Some(&name))?.clone();
            let link = relink(ctx, &p.id, &p.name, &env.name)?;
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
                    selected: current.as_deref() == Some(e.id.as_str()),
                })
                .collect();
            ctx.out.either(&rows, |o| {
                let mut t = Table::new(&["environment", "id", ""]);
                for r in &rows {
                    t.row([
                        r.name.as_str(),
                        &r.id,
                        if r.selected { "(selected)" } else { "" },
                    ]);
                }
                o.table(&t);
            })
        }
    }
}
