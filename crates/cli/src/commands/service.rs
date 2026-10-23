//! `add`: create a service from a database template, an image or a repository.

use serde_json::{Value, json};

use crate::api::NewService;
use crate::api::endpoints::VariableScope;
use crate::cli::AddArgs;
use crate::commands::variables::parse_pairs;
use crate::context::Ctx;
use crate::output::usage;

/// A valid service name from free text: lowercase letters, digits and `-`,
/// starting with a letter, at most 63 characters.
pub fn service_name(raw: &str) -> String {
    let mut name = String::new();
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            name.push(c);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_matches('-');
    let name = if name.starts_with(|c: char| c.is_ascii_lowercase()) {
        name.to_owned()
    } else {
        format!("svc-{name}")
    };
    name.chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned()
}

/// `ghcr.io/acme/api:1.2` → `api`; `nginx@sha256:…` → `nginx`.
pub fn name_from_image(image: &str) -> String {
    let last = image.rsplit('/').next().unwrap_or(image);
    let last = last.split('@').next().unwrap_or(last);
    service_name(last.split(':').next().unwrap_or(last))
}

/// `acme/My_App` → `my-app`.
pub fn name_from_repo(repo: &str) -> String {
    service_name(repo.rsplit('/').next().unwrap_or(repo))
}

/// The request body for `args`.
pub fn new_service(args: &AddArgs) -> anyhow::Result<NewService> {
    let source = match (&args.database, &args.image, &args.repo) {
        (Some(db), _, _) => {
            return Ok(NewService {
                name: args
                    .name
                    .clone()
                    .unwrap_or_else(|| db.template().to_owned()),
                template: Some(db.template().to_owned()),
                config: None,
            });
        }
        (_, Some(image), _) => (
            name_from_image(image),
            json!({ "type": "image", "image": image }),
        ),
        (_, _, Some(repo)) => {
            if repo.split('/').count() != 2 || repo.starts_with('/') || repo.ends_with('/') {
                return Err(usage("--repo must be owner/name"));
            }
            (
                name_from_repo(repo),
                json!({ "type": "repo", "repo": repo, "branch": args.branch }),
            )
        }
        _ => return Err(usage("pass --database, --image or --repo")),
    };
    let (default_name, source) = source;
    let mut config = json!({ "source": source });
    if let Some(port) = args.port {
        config["port"] = Value::from(port);
    }
    Ok(NewService {
        name: args.name.clone().unwrap_or(default_name),
        template: None,
        config: Some(config),
    })
}

pub async fn add(ctx: &mut Ctx, args: AddArgs) -> anyhow::Result<()> {
    let vars = parse_pairs(&args.variables)?;
    let req = new_service(&args)?;
    let api = ctx.api()?;
    let p = ctx.project().await?;
    let created = api.create_service(&p.id, &req).await?;
    let service_id = created["id"].as_str().unwrap_or_default().to_owned();
    let env = ctx.environment(&p)?.clone();
    if !vars.is_empty() {
        let instance = created["instances"]
            .as_array()
            .and_then(|is| {
                is.iter()
                    .find(|i| i["environmentId"].as_str() == Some(env.id.as_str()))
            })
            .and_then(|i| i["id"].as_str())
            .ok_or_else(|| anyhow::anyhow!("the new service has no instance in {}", env.name))?;
        let changes = crate::api::VariableChanges {
            set: vars,
            ..Default::default()
        };
        api.update_variables(VariableScope::Instance(instance), &changes)
            .await?;
    }
    // A new app service becomes this directory's default (like Railway).
    let mut selected = false;
    if req.template.is_none()
        && let Some(mut link) = ctx.linked().filter(|l| l.project == p.id).cloned()
    {
        link.service = Some(service_id.clone());
        link.service_name = Some(req.name.clone());
        ctx.save_link(link)?;
        selected = true;
    }
    ctx.out.either(&created, |o| {
        o.line(format!("Created {} in {}", req.name, p.name));
        if selected {
            o.note(format!(
                "Selected {} as this directory's service.",
                req.name
            ));
        }
        o.note("Deploy it with `modula deploy -s <name>` (or `up`).");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser;

    fn add_args(args: &[&str]) -> AddArgs {
        let cli =
            Cli::try_parse_from(["modula", "add"].into_iter().chain(args.iter().copied())).unwrap();
        match cli.command {
            Command::Add(a) => a,
            _ => unreachable!(),
        }
    }

    #[test]
    fn names_are_derived_and_valid() {
        assert_eq!(name_from_image("ghcr.io/acme/api:1.2"), "api");
        assert_eq!(name_from_image("nginx@sha256:abc"), "nginx");
        assert_eq!(name_from_image("localhost:5000/My_App"), "my-app");
        assert_eq!(name_from_repo("acme/Web.Site"), "web-site");
        assert_eq!(service_name("1password"), "svc-1password");
        assert_eq!(service_name("--x--"), "x");
        assert!(service_name(&"a".repeat(80)).len() <= 63);
    }

    #[test]
    fn request_bodies() {
        let r = new_service(&add_args(&["--database", "mongo"])).unwrap();
        assert_eq!(r.name, "mongo");
        assert_eq!(r.template.as_deref(), Some("mongo"));
        assert!(r.config.is_none());

        let r = new_service(&add_args(&["--image", "nginx:alpine", "--port", "80"])).unwrap();
        assert_eq!(r.name, "nginx");
        assert_eq!(
            r.config.unwrap(),
            json!({ "source": { "type": "image", "image": "nginx:alpine" }, "port": 80 })
        );

        let r = new_service(&add_args(&["--repo", "acme/api", "--name", "backend"])).unwrap();
        assert_eq!(r.name, "backend");
        assert_eq!(
            r.config.unwrap(),
            json!({ "source": { "type": "repo", "repo": "acme/api", "branch": "main" } })
        );
        assert!(new_service(&add_args(&["--repo", "acme"])).is_err());
    }
}
