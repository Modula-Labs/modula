//! Turning names (or ids) from flags and `.modula/project.toml` into the
//! project, environment and service instance to act on.

use anyhow::{Context, bail};

use crate::api::{Api, Environment, Project, ProjectDetail, Service, Workspace, api_error};

/// The default environment when none is given or linked.
pub const DEFAULT_ENVIRONMENT: &str = "production";

/// Ids are 16 lowercase base36 characters (see crates/id).
pub fn looks_like_id(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

fn names<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let v: Vec<&str> = items.collect();
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

/// Pick a project by id or (case-insensitive) name; with no name, the only one.
pub fn pick_project<'a>(
    projects: &'a [(String, Project)],
    wanted: Option<&str>,
) -> anyhow::Result<&'a Project> {
    let all = || names(projects.iter().map(|(_, p)| p.name.as_str()));
    match wanted {
        Some(w) => {
            let matches: Vec<&(String, Project)> = projects
                .iter()
                .filter(|(_, p)| p.id == w || p.name.eq_ignore_ascii_case(w))
                .collect();
            match matches[..] {
                [(_, p)] => Ok(p),
                [] => bail!("no project named {w} (projects: {})", all()),
                _ => bail!(
                    "several projects are named {w} (in {}); pass its id instead",
                    names(matches.iter().map(|(ws, _)| ws.as_str()))
                ),
            }
        }
        None => match projects {
            [(_, p)] => Ok(p),
            [] => bail!("you have no projects yet"),
            _ => bail!("pass --project or run `modula link` (projects: {})", all()),
        },
    }
}

/// Pick an environment by name or id; with no name, `production`, else the only one.
pub fn pick_environment<'a>(
    envs: &'a [Environment],
    wanted: Option<&str>,
) -> anyhow::Result<&'a Environment> {
    let found = match wanted {
        Some(w) => envs
            .iter()
            .find(|e| e.id == w || e.name.eq_ignore_ascii_case(w)),
        None => envs
            .iter()
            .find(|e| e.name == DEFAULT_ENVIRONMENT)
            .or(match envs {
                [only] => Some(only),
                _ => None,
            }),
    };
    found.with_context(|| {
        format!(
            "no environment {} (environments: {})",
            wanted.unwrap_or(DEFAULT_ENVIRONMENT),
            names(envs.iter().map(|e| e.name.as_str()))
        )
    })
}

/// Pick a service by name or id; with no name, the only database service.
pub fn pick_service<'a>(
    services: &'a [Service],
    wanted: Option<&str>,
) -> anyhow::Result<&'a Service> {
    if let Some(w) = wanted {
        return services
            .iter()
            .find(|s| s.id == w || s.name.eq_ignore_ascii_case(w))
            .with_context(|| {
                format!(
                    "no service named {w} (services: {})",
                    names(services.iter().map(|s| s.name.as_str()))
                )
            });
    }
    let dbs: Vec<&Service> = services
        .iter()
        .filter(|s| {
            matches!(
                s.template.as_deref(),
                Some("postgres" | "mysql" | "redis" | "mongo")
            )
        })
        .collect();
    match dbs[..] {
        [only] => Ok(only),
        [] => bail!(
            "this project has no database service; name the service to connect to (services: {})",
            names(services.iter().map(|s| s.name.as_str()))
        ),
        _ => bail!(
            "this project has several databases; name one: {}",
            names(dbs.iter().map(|s| s.name.as_str()))
        ),
    }
}

/// The service most commands act on: the one named, else the linked one,
/// else the project's only service.
pub fn default_service<'a>(
    services: &'a [Service],
    wanted: Option<&str>,
) -> anyhow::Result<&'a Service> {
    if wanted.is_some() {
        return pick_service(services, wanted);
    }
    match services {
        [only] => Ok(only),
        [] => bail!("this project has no services yet; add one with `modula add`"),
        _ => bail!(
            "pass --service or run `modula service <name>` (services: {})",
            names(services.iter().map(|s| s.name.as_str()))
        ),
    }
}

/// Pick a workspace by id or name; with no name, the only one.
pub fn pick_workspace<'a>(
    workspaces: &'a [Workspace],
    wanted: Option<&str>,
) -> anyhow::Result<&'a Workspace> {
    let all = || names(workspaces.iter().map(|w| w.name.as_str()));
    match wanted {
        Some(w) => workspaces
            .iter()
            .find(|ws| ws.id == w || ws.name.eq_ignore_ascii_case(w))
            .with_context(|| format!("no workspace named {w} (workspaces: {})", all())),
        None => match workspaces {
            [only] => Ok(only),
            [] => bail!("you aren't a member of any workspace"),
            _ => bail!("pass --workspace (workspaces: {})", all()),
        },
    }
}

/// Every project the token can see, with its workspace's name.
pub async fn all_projects(api: &Api) -> anyhow::Result<Vec<(String, Project)>> {
    let me = api.me().await?;
    let mut out = Vec::new();
    for ws in me.workspaces {
        let projects = api.projects(&ws.id).await?;
        out.extend(projects.into_iter().map(|p| (ws.name.clone(), p)));
    }
    Ok(out)
}

/// Load a project by id (fast path) or by name.
pub async fn project(api: &Api, wanted: Option<&str>) -> anyhow::Result<ProjectDetail> {
    if let Some(w) = wanted.filter(|w| looks_like_id(w)) {
        match api.project(w).await {
            Ok(p) => return Ok(p),
            Err(e) if api_error(&e).is_some_and(|a| a.status == 404) => {}
            Err(e) => return Err(e),
        }
    }
    let projects = all_projects(api).await?;
    let p = pick_project(&projects, wanted)?;
    api.project(&p.id).await
}

/// The service instance of `service` in `env`.
pub fn instance_id(
    p: &ProjectDetail,
    service: &Service,
    env: &Environment,
) -> anyhow::Result<String> {
    p.instances
        .iter()
        .find(|i| i.service_id == service.id && i.environment_id == env.id)
        .map(|i| i.id.clone())
        .with_context(|| format!("{} has no instance in {}", service.name, env.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(id: &str, name: &str) -> Environment {
        Environment {
            id: id.into(),
            name: name.into(),
        }
    }

    fn svc(name: &str, template: Option<&str>) -> Service {
        Service {
            id: format!("id-{name}"),
            name: name.into(),
            template: template.map(Into::into),
        }
    }

    fn proj(id: &str, name: &str) -> Project {
        Project {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn ids_are_recognized() {
        assert!(looks_like_id("0a1b2c3d4e5f6g7h"));
        assert!(!looks_like_id("shop"));
        assert!(!looks_like_id("0A1B2C3D4E5F6G7H"));
    }

    #[test]
    fn environments_default_to_production() {
        let envs = [env("1", "staging"), env("2", "production")];
        assert_eq!(pick_environment(&envs, None).unwrap().id, "2");
        assert_eq!(pick_environment(&envs, Some("Staging")).unwrap().id, "1");
        assert_eq!(
            pick_environment(&envs, Some("2")).unwrap().name,
            "production"
        );
        let err = pick_environment(&envs, Some("dev"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("staging, production"), "{err}");
        // A single non-production environment is the default too.
        assert_eq!(pick_environment(&[env("3", "dev")], None).unwrap().id, "3");
    }

    #[test]
    fn the_only_database_is_the_default_service() {
        let one = [svc("web", None), svc("postgres", Some("postgres"))];
        assert_eq!(pick_service(&one, None).unwrap().name, "postgres");
        assert_eq!(pick_service(&one, Some("WEB")).unwrap().name, "web");
        let two = [svc("pg", Some("postgres")), svc("cache", Some("redis"))];
        let err = pick_service(&two, None).unwrap_err().to_string();
        assert!(err.contains("pg, cache"), "{err}");
        assert!(pick_service(&[svc("web", None)], None).is_err());
        assert!(pick_service(&one, Some("nope")).is_err());
    }

    #[test]
    fn the_default_service_is_named_or_the_only_one() {
        let one = [svc("web", None)];
        assert_eq!(default_service(&one, None).unwrap().name, "web");
        let two = [svc("web", None), svc("postgres", Some("postgres"))];
        let err = default_service(&two, None).unwrap_err().to_string();
        assert!(err.contains("web, postgres"), "{err}");
        assert_eq!(
            default_service(&two, Some("id-postgres")).unwrap().name,
            "postgres"
        );
        assert!(default_service(&[], None).is_err());
    }

    #[test]
    fn workspaces_by_name_or_id() {
        let ws = |id: &str, name: &str| Workspace {
            id: id.into(),
            name: name.into(),
            role: "owner".into(),
        };
        let all = [ws("1", "Team"), ws("2", "Personal")];
        assert_eq!(pick_workspace(&all, Some("team")).unwrap().id, "1");
        assert_eq!(pick_workspace(&all, Some("2")).unwrap().name, "Personal");
        assert!(pick_workspace(&all, None).is_err());
        assert_eq!(pick_workspace(&all[..1], None).unwrap().id, "1");
    }

    #[test]
    fn projects_by_name_or_id() {
        let ps = vec![
            ("team".to_owned(), proj("aaaaaaaaaaaaaaaa", "shop")),
            ("team".to_owned(), proj("bbbbbbbbbbbbbbbb", "blog")),
            ("other".to_owned(), proj("cccccccccccccccc", "Shop")),
        ];
        assert_eq!(
            pick_project(&ps, Some("blog")).unwrap().id,
            "bbbbbbbbbbbbbbbb"
        );
        assert_eq!(
            pick_project(&ps, Some("cccccccccccccccc")).unwrap().name,
            "Shop"
        );
        let err = pick_project(&ps, Some("shop")).unwrap_err().to_string();
        assert!(err.contains("team, other"), "{err}");
        assert!(pick_project(&ps, None).is_err());
        assert_eq!(pick_project(&ps[1..2], None).unwrap().name, "blog");
    }
}
