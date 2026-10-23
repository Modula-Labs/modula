//! Projects and the directory link: `list`, `init`, `link`, `unlink`,
//! `status`, `open`, `service`.

use serde::Serialize;
use url::Url;

use crate::api::{Environment, ProjectDetail};
use crate::config::Link;
use crate::context::Ctx;
use crate::output::{Table, or_dash};
use crate::resolve;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRow {
    workspace: String,
    id: String,
    name: String,
    linked: bool,
}

pub async fn list(ctx: &Ctx) -> anyhow::Result<()> {
    let projects = resolve::all_projects(ctx.api()?).await?;
    let linked = ctx.linked().map(|l| l.project.as_str());
    let rows: Vec<ProjectRow> = projects
        .into_iter()
        .map(|(workspace, p)| ProjectRow {
            linked: Some(p.id.as_str()) == linked,
            workspace,
            id: p.id,
            name: p.name,
        })
        .collect();
    ctx.out.either(&rows, |o| {
        if rows.is_empty() {
            o.line("No projects yet; create one with `modula-deploy init <name>`.");
            return;
        }
        let mut t = Table::new(&["workspace", "project", "id", ""]);
        for r in &rows {
            t.row([
                r.workspace.as_str(),
                &r.name,
                &r.id,
                if r.linked { "(linked)" } else { "" },
            ]);
        }
        o.table(&t);
    })
}

pub async fn init(ctx: &mut Ctx, name: &str, workspace: Option<&str>) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let me = api.me().await?;
    let ws = resolve::pick_workspace(&me.workspaces, workspace)?;
    let p = api.create_project(&ws.id, name).await?;
    let env = resolve::pick_environment(&p.environments, None)?;
    // Link here, not in a parent's linked directory.
    ctx.link = None;
    let file = ctx.save_link(Link::new(&p.id, &p.name, &env.name))?;
    ctx.out.either(&p, |o| {
        o.line(format!(
            "Created {} in {} and linked it ({})",
            p.name, ws.name, env.name
        ));
        o.note(format!("Wrote {}", file.display()));
    })
}

pub async fn link(ctx: &mut Ctx) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let p = resolve::project(api, ctx.global.project.as_deref()).await?;
    let env = resolve::pick_environment(&p.environments, ctx.global.environment.as_deref())?;
    let mut link = Link::new(&p.id, &p.name, &env.name);
    if let Some(s) = ctx.global.service.as_deref() {
        let s = resolve::pick_service(&p.services, Some(s))?;
        link.service = Some(s.id.clone());
        link.service_name = Some(s.name.clone());
    }
    // Link here, not in a parent's linked directory.
    ctx.link = None;
    let file = ctx.save_link(link.clone())?;
    let cwd = &ctx.session.cwd;
    ctx.out.either(&link, |o| {
        o.line(format!(
            "Linked {} to {} ({})",
            cwd.display(),
            p.name,
            env.name
        ));
        o.note(format!("Wrote {}", file.display()));
    })
}

pub fn unlink(ctx: &Ctx) -> anyhow::Result<()> {
    let removed = match &ctx.link {
        Some((file, _)) => {
            std::fs::remove_file(file)
                .map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", file.display()))?;
            if let Some(dir) = file.parent() {
                let _ = std::fs::remove_dir(dir); // only if now empty
            }
            Some(file.display().to_string())
        }
        None => None,
    };
    ctx.out.either(
        &serde_json::json!({ "removed": removed }),
        |o| match &removed {
            Some(f) => o.line(format!("Unlinked ({f} removed)")),
            None => o.line("This directory isn't linked."),
        },
    )
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub id: String,
    pub name: String,
    pub instance_id: String,
    /// The database template, if any.
    pub template: Option<String>,
    /// The image, `owner/repo`, or the template.
    pub source: Option<String>,
    /// The newest deployment's status; None = never deployed.
    pub status: Option<String>,
    pub latest_deployment_id: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    project: Named,
    environment: Named,
    /// The service commands default to, if one is selected.
    service: Option<String>,
    services: Vec<ServiceStatus>,
    /// Changes waiting for a deploy (None if the role can't see them).
    pending_changes: Option<usize>,
}

#[derive(Debug, Serialize)]
struct Named {
    id: String,
    name: String,
}

/// Every service of `p` in `env`, with its latest deployment.
pub fn service_statuses(p: &ProjectDetail, env: &Environment) -> Vec<ServiceStatus> {
    p.services
        .iter()
        .filter_map(|s| {
            let i = p
                .instances
                .iter()
                .find(|i| i.service_id == s.id && i.environment_id == env.id)?;
            let source = match i.source_type.as_deref() {
                _ if s.template.is_some() => s.template.clone(),
                Some("repo") => i.repo.clone(),
                _ => i.image.clone(),
            };
            Some(ServiceStatus {
                id: s.id.clone(),
                name: s.name.clone(),
                instance_id: i.id.clone(),
                template: s.template.clone(),
                source,
                status: i.latest_deployment_status.clone(),
                latest_deployment_id: i.latest_deployment_id.clone(),
                url: i.domain.as_ref().map(|d| format!("https://{d}")),
            })
        })
        .collect()
}

pub async fn status(ctx: &Ctx) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let p = ctx.project().await?;
    let env = ctx.environment(&p)?;
    let selected = ctx
        .wanted_service(&p)
        .and_then(|w| resolve::pick_service(&p.services, Some(w)).ok())
        .map(|s| s.name.clone());
    let pending = api.pending(&env.id).await.ok().map(|p| p.count);
    let status = Status {
        project: Named {
            id: p.id.clone(),
            name: p.name.clone(),
        },
        environment: Named {
            id: env.id.clone(),
            name: env.name.clone(),
        },
        service: selected,
        services: service_statuses(&p, env),
        pending_changes: pending,
    };
    ctx.out.either(&status, |o| {
        o.line(format!("Project      {}", p.name));
        o.line(format!("Environment  {}", env.name));
        if let Some(s) = &status.service {
            o.line(format!("Service      {s}"));
        }
        if let Some(n) = status.pending_changes.filter(|n| *n > 0) {
            o.line(format!(
                "Pending      {n} change{} (deploy with `modula-deploy deploy`)",
                if n == 1 { "" } else { "s" }
            ));
        }
        if status.services.is_empty() {
            o.line("\nNo services yet; add one with `modula-deploy add`.");
            return;
        }
        o.line("");
        let mut t =
            Table::new(&["service", "status", "url", "source", "deployment"]).status_column(1);
        for s in &status.services {
            t.row([
                s.name.clone(),
                s.status.clone().unwrap_or_else(|| "not deployed".into()),
                or_dash(s.url.as_deref()),
                or_dash(s.source.as_deref()),
                or_dash(s.latest_deployment_id.as_deref()),
            ]);
        }
        o.table(&t);
    })
}

/// The dashboard page of a project (and environment and service).
pub fn dashboard_url(
    base: &Url,
    project: &str,
    environment: &str,
    service: Option<&str>,
) -> anyhow::Result<Url> {
    let mut url = base.join(&format!("projects/{project}"))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("env", environment);
        if let Some(s) = service {
            q.append_pair("service", s);
        }
    }
    Ok(url)
}

pub async fn open(ctx: &Ctx) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let p = ctx.project().await?;
    let env = ctx.environment(&p)?;
    let service = ctx
        .wanted_service(&p)
        .map(|w| resolve::pick_service(&p.services, Some(w)))
        .transpose()?;
    let url = dashboard_url(
        &api.login.url,
        &p.id,
        &env.id,
        service.map(|s| s.id.as_str()),
    )?;
    ctx.out
        .either(&serde_json::json!({ "url": url.as_str() }), |o| {
            o.line(url.as_str());
        })?;
    if !ctx.out.json && !ctx.out.is_captured() && ctx.session.interactive {
        open_in_browser(url.as_str());
    }
    Ok(())
}

/// Best effort; the URL is printed either way.
fn open_in_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ServiceRow {
    id: String,
    name: String,
    template: Option<String>,
    selected: bool,
}

pub async fn service(ctx: &mut Ctx, name: Option<&str>) -> anyhow::Result<()> {
    let p = ctx.project().await?;
    let Some(name) = name else {
        let selected = ctx.wanted_service(&p).map(str::to_owned);
        let rows: Vec<ServiceRow> = p
            .services
            .iter()
            .map(|s| ServiceRow {
                id: s.id.clone(),
                name: s.name.clone(),
                template: s.template.clone(),
                selected: selected
                    .as_deref()
                    .is_some_and(|w| w == s.id || w.eq_ignore_ascii_case(&s.name)),
            })
            .collect();
        return ctx.out.either(&rows, |o| {
            let mut t = Table::new(&["service", "template", "id", ""]);
            for r in &rows {
                t.row([
                    r.name.as_str(),
                    r.template.as_deref().unwrap_or("-"),
                    &r.id,
                    if r.selected { "(selected)" } else { "" },
                ]);
            }
            o.table(&t);
        });
    };
    let s = resolve::pick_service(&p.services, Some(name))?.clone();
    let env = ctx.environment(&p)?.name.clone();
    let mut link = match ctx.linked() {
        Some(l) if l.project == p.id => l.clone(),
        _ => Link::new(&p.id, &p.name, &env),
    };
    link.service = Some(s.id.clone());
    link.service_name = Some(s.name.clone());
    ctx.save_link(link.clone())?;
    ctx.out.either(&link, |o| {
        o.line(format!("Selected {} ({})", s.name, p.name));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Instance, Service};

    #[test]
    fn statuses_come_from_the_environment_instances() {
        let env = |id: &str| Environment {
            id: id.into(),
            name: id.into(),
        };
        let p = ProjectDetail {
            id: "p".into(),
            name: "shop".into(),
            environments: vec![env("prod"), env("stage")],
            services: vec![
                Service {
                    id: "s1".into(),
                    name: "api".into(),
                    template: None,
                },
                Service {
                    id: "s2".into(),
                    name: "postgres".into(),
                    template: Some("postgres".into()),
                },
            ],
            instances: vec![
                Instance {
                    id: "i1".into(),
                    service_id: "s1".into(),
                    environment_id: "prod".into(),
                    latest_deployment_id: Some("d1".into()),
                    latest_deployment_status: Some("active".into()),
                    domain: Some("api.example.com".into()),
                    source_type: Some("repo".into()),
                    repo: Some("acme/api".into()),
                    ..Default::default()
                },
                Instance {
                    id: "i2".into(),
                    service_id: "s2".into(),
                    environment_id: "prod".into(),
                    source_type: Some("image".into()),
                    image: Some("postgres:17".into()),
                    ..Default::default()
                },
                Instance {
                    id: "i3".into(),
                    service_id: "s1".into(),
                    environment_id: "stage".into(),
                    ..Default::default()
                },
            ],
        };
        let rows = service_statuses(&p, &env("prod"));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].instance_id, "i1");
        assert_eq!(rows[0].url.as_deref(), Some("https://api.example.com"));
        assert_eq!(rows[0].source.as_deref(), Some("acme/api"));
        assert_eq!(rows[1].source.as_deref(), Some("postgres"));
        assert_eq!(rows[1].status, None);
        assert_eq!(service_statuses(&p, &env("stage")).len(), 1);
    }

    #[test]
    fn dashboard_urls_use_ids() {
        let base = Url::parse("https://cloud.example.com/").unwrap();
        let u = dashboard_url(&base, "p1", "e1", Some("s1")).unwrap();
        assert_eq!(
            u.as_str(),
            "https://cloud.example.com/projects/p1?env=e1&service=s1"
        );
        let u = dashboard_url(&base, "p1", "e1", None).unwrap();
        assert_eq!(u.as_str(), "https://cloud.example.com/projects/p1?env=e1");
    }
}
