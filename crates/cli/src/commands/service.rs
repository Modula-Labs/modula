//! `add`: create a service from a database template, an image, a
//! repository or a function file, or an app template's services.

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

/// `rename NEW`: rename the selected service, like the dashboard: every
/// `${{old.VAR}}` in the project becomes `${{new.VAR}}` (pending changes on
/// the services that reference it).
pub async fn rename(ctx: &mut Ctx, new: &str) -> anyhow::Result<()> {
    let name = service_name(new);
    let t = ctx.target().await?;
    let id = t.service.id.clone();
    let api = ctx.api()?;
    let preview: Value = api
        .get(&format!("/api/services/{id}/rename-preview"))
        .await?;
    let count = |key: &str| preview[key].as_array().map_or(0, Vec::len);
    ctx.confirm(&format!(
        "Rename {} to {name}? References in {} variable(s) will be rewritten.",
        t.service.name,
        count("references")
    ))?;
    let renamed: Value = api
        .patch(&format!("/api/services/{id}"), &json!({ "name": name }))
        .await?;
    // Keep the directory's link showing the current name.
    if let Some(link) = ctx
        .linked()
        .filter(|l| l.service.as_deref() == Some(id.as_str()))
    {
        let mut link = link.clone();
        link.service_name = Some(name.clone());
        ctx.save_link(link)?;
    }
    let mut similar: Vec<String> = preview["similarNames"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["name"].as_str().map(str::to_owned))
        .collect();
    similar.sort();
    similar.dedup();
    ctx.out.either(&renamed, |o| {
        o.line(format!(
            "Renamed {} to {name}; rewrote references in {} variable(s)",
            t.service.name, renamed["rewrittenVariables"]
        ));
        if !similar.is_empty() {
            o.line(format!(
                "Variable names are unchanged: {}",
                similar.join(", ")
            ));
        }
    })
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

/// Inline function code is at most this big (the server's limit).
const MAX_FUNCTION_BYTES: usize = 1024 * 1024;

/// The request body for `args`.
pub fn new_service(args: &AddArgs) -> anyhow::Result<NewService> {
    if args.cloudflare && args.function.is_none() {
        return Err(usage("--cloudflare is for --function"));
    }
    if let Some(file) = &args.function {
        let code = std::fs::read_to_string(file)
            .map_err(|e| usage(format!("cannot read {}: {e}", file.display())))?;
        if code.len() > MAX_FUNCTION_BYTES {
            return Err(usage(
                "a function file is at most 1 MB: deploy bigger functions from a repository in the dashboard",
            ));
        }
        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !matches!(*s, "index" | "worker" | "main"))
            .unwrap_or("function");
        let mut config = json!({
            "source": { "type": "function", "code": code, "compatibilityDate": args.compatibility_date },
        });
        if args.cloudflare {
            config["hosting"] = json!({ "type": "cloudflare" });
        }
        return Ok(NewService {
            name: args.name.clone().unwrap_or_else(|| service_name(stem)),
            template: None,
            config: Some(config),
        });
    }
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
    if let Some(template) = &args.template {
        return add_app(ctx, template, args.name.as_deref()).await;
    }
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

/// `add --template`: create an app's services and deploy them in the
/// selected environment, databases first (like the dashboard).
async fn add_app(ctx: &mut Ctx, template: &str, prefix: Option<&str>) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let gallery = api.app_templates().await?;
    let Some(app) = gallery.iter().find(|a| a["id"] == template) else {
        let mut ids: Vec<&str> = gallery.iter().filter_map(|a| a["id"].as_str()).collect();
        ids.sort_unstable();
        if template == "list" {
            return ctx.out.either(&gallery, |o| {
                for a in &gallery {
                    o.line(format!(
                        "{:<12} {} ({} services)",
                        a["id"].as_str().unwrap_or_default(),
                        a["description"].as_str().unwrap_or_default(),
                        a["services"].as_array().map_or(0, Vec::len),
                    ));
                }
            });
        }
        return Err(usage(format!(
            "unknown template {template}; one of: {}",
            ids.join(", ")
        )));
    };
    let p = ctx.project().await?;
    let env = ctx.environment(&p)?.clone();
    let mut created = api
        .create_app(
            &p.id,
            template,
            &env.id,
            prefix.map(service_name).as_deref(),
        )
        .await?;
    let mut warnings: Vec<String> = created["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w.as_str().map(str::to_owned))
        .collect();
    let services = created["services"].as_array().cloned().unwrap_or_default();
    let mut deployments = Vec::new();
    for s in &services {
        let instance = s["instanceId"].as_str().unwrap_or_default();
        match api.deploy_instance(instance).await {
            Ok(d) => deployments.push(json!({ "service": s["name"], "deployment": d.id })),
            // Without a server the services wait; deploy them later.
            Err(e) => warnings.push(format!("{}: {e:#}", s["name"].as_str().unwrap_or_default())),
        }
    }
    created["deployments"] = Value::from(deployments);
    created["warnings"] = json!(warnings);
    let name = app["name"].as_str().unwrap_or(template);
    ctx.out.either(&created, |o| {
        o.line(format!("Created {name} in {} ({}):", p.name, env.name));
        for s in &services {
            o.line(format!("  {}", s["name"].as_str().unwrap_or_default()));
        }
        if let Some(d) = created["domain"].as_str() {
            o.line(format!("Open https://{d} once it is online."));
        }
        for w in &warnings {
            o.note(format!("warning: {w}"));
        }
        o.note("Follow the deployments with `modula status`.");
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

        let dir = std::env::temp_dir().join(format!("modula-cli-fn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello_world.js");
        std::fs::write(
            &file,
            "export default { fetch() { return new Response('hi'); } };",
        )
        .unwrap();
        let path = file.to_str().unwrap();
        let r = new_service(&add_args(&["--function", path, "--cloudflare"])).unwrap();
        assert_eq!(r.name, "hello-world");
        let c = r.config.unwrap();
        assert_eq!(c["source"]["type"], "function");
        assert!(
            c["source"]["code"]
                .as_str()
                .unwrap()
                .contains("Response('hi')")
        );
        assert_eq!(
            c["source"]["compatibilityDate"],
            crate::cli::DEFAULT_COMPATIBILITY_DATE
        );
        assert_eq!(c["hosting"]["type"], "cloudflare");
        let index = dir.join("index.js");
        std::fs::write(&index, "export default {};").unwrap();
        let r = new_service(&add_args(&["--function", index.to_str().unwrap()])).unwrap();
        assert_eq!(r.name, "function");
        assert!(r.config.unwrap().get("hosting").is_none(), "your servers");
        assert!(
            new_service(&add_args(&[
                "--function",
                dir.join("nope.js").to_str().unwrap()
            ]))
            .is_err()
        );
        assert!(new_service(&add_args(&["--image", "nginx", "--cloudflare"])).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
