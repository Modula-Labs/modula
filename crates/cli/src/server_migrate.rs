//! `modula server migrate`: move everything on a server to a new one or an
//! existing one, over the same API the dashboard uses.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context as _, bail};
use serde_json::{Value, json};

use crate::context::Ctx;
use crate::output::usage;

const POLL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, clap::Args)]
pub struct ServerMigrateArgs {
    /// The server to move away from: name or id.
    pub server: String,
    /// Move to this existing server (name or id).
    #[arg(long, value_name = "SERVER", conflicts_with_all = ["location", "server_type", "disk_gb"])]
    pub to: Option<String>,
    /// Where to create the new server: a catalog id, e.g. hetzner:hel1 or
    /// aws:us-east-1. The provider is its prefix.
    #[arg(long, requires = "server_type")]
    pub location: Option<String>,
    /// The new server's type, e.g. cx22 or t3.small.
    #[arg(long = "server-type", value_name = "TYPE", requires = "location")]
    pub server_type: Option<String>,
    /// Root disk size in GB, where it is chosen apart from the type (EC2).
    #[arg(long, requires = "location")]
    pub disk_gb: Option<u32>,
    /// The workspace to act for (name or id): needed for a shared server
    /// added by hand.
    #[arg(long)]
    pub workspace: Option<String>,
    /// A DNS record id (from the preview) to leave on the old address.
    /// Repeatable.
    #[arg(long = "skip-dns", value_name = "RECORD")]
    pub skip_dns: Vec<String>,
    /// Show what would happen and stop.
    #[arg(long)]
    pub preview: bool,
    /// Start it and don't wait.
    #[arg(short, long)]
    pub detach: bool,
    /// Retry this server's migration whose DNS step failed, and follow it.
    #[arg(long, conflicts_with_all = ["to", "location", "preview"])]
    pub retry_dns: bool,
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

/// The id of the entry named or identified by `wanted`.
fn pick_id(items: &[Value], wanted: &str, what: &str) -> anyhow::Result<String> {
    items
        .iter()
        .find(|n| s(n, "id") == wanted || s(n, "name") == wanted)
        .map(|n| s(n, "id").to_owned())
        .with_context(|| format!("no {what} named {wanted}"))
}

/// The JSON body's target from the flags.
fn target(args: &ServerMigrateArgs, node: Option<&str>) -> anyhow::Result<Value> {
    if let Some(id) = node {
        return Ok(json!({ "kind": "existing", "nodeId": id }));
    }
    let (Some(location), Some(server_type)) = (&args.location, &args.server_type) else {
        return Err(usage(
            "say where to: --to SERVER, or --location L --server-type T",
        ));
    };
    let Some((provider, _)) = location.split_once(':') else {
        return Err(usage(
            "--location is a catalog id with its provider, e.g. hetzner:hel1",
        ));
    };
    let mut t = json!({
        "kind": "new",
        "provider": provider,
        "location": location,
        "serverType": server_type,
    });
    if let Some(gb) = args.disk_gb {
        t["diskGb"] = json!(gb);
    }
    Ok(t)
}

fn preview_query(target: &Value, workspace: Option<&str>) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    if let Some(w) = workspace {
        q.append_pair("workspaceId", w);
    }
    match s(target, "kind") {
        "existing" => {
            q.append_pair("targetNodeId", s(target, "nodeId"));
        }
        _ => {
            q.append_pair("location", s(target, "location"));
            q.append_pair("serverType", s(target, "serverType"));
            if let Some(gb) = target["diskGb"].as_u64() {
                q.append_pair("diskGb", &gb.to_string());
            }
        }
    }
    q.finish()
}

fn print_preview(ctx: &Ctx, p: &Value) {
    let out = &ctx.out;
    out.line(format!("Migrating {}:", s(p, "name")));
    for service in p["services"].as_array().into_iter().flatten() {
        let volumes = service["volumes"].as_array().map_or(0, Vec::len);
        out.line(format!(
            "  {} ({}/{}){}",
            s(service, "service"),
            s(service, "project"),
            s(service, "environment"),
            if volumes > 0 {
                " - down while its data copies"
            } else {
                ""
            }
        ));
    }
    for r in p["dns"]["records"].as_array().into_iter().flatten() {
        out.line(format!(
            "  DNS {} -> repointed (record {}; --skip-dns to keep)",
            s(r, "name"),
            s(r, "id")
        ));
    }
    for w in p["warnings"].as_array().into_iter().flatten() {
        let label = if w["blocking"] == true {
            "blocked"
        } else {
            "warning"
        };
        out.line(format!("  {label}: {}", s(w, "message")));
    }
}

pub async fn run(ctx: &mut Ctx, args: ServerMigrateArgs) -> anyhow::Result<i32> {
    let api = ctx.api()?;
    let nodes: Vec<Value> = api.get("/api/nodes").await?;
    let node = pick_id(&nodes, &args.server, "server")?;

    if args.retry_dns {
        let all: Vec<Value> = api.get(&format!("/api/nodes/{node}/migrations")).await?;
        let id = all
            .iter()
            .find(|m| s(m, "status") == "failed" && s(m, "step") == "dns")
            .map(|m| s(m, "id").to_owned())
            .context("this server has no migration whose DNS step failed")?;
        let _: Value = api
            .post_empty(&format!("/api/server-migrations/{id}/retry-dns"))
            .await?;
        return follow(ctx, &id, &node).await;
    }

    let workspace = match &args.workspace {
        Some(w) => {
            let me: Value = api.get("/api/auth/me").await?;
            let all = me["workspaces"].as_array().cloned().unwrap_or_default();
            Some(pick_id(&all, w, "workspace")?)
        }
        None => None,
    };
    let to = match &args.to {
        Some(t) => Some(pick_id(&nodes, t, "server")?),
        None => None,
    };
    let target = target(&args, to.as_deref())?;
    let query = preview_query(&target, workspace.as_deref());
    let preview: Value = api
        .get(&format!("/api/nodes/{node}/migration-preview?{query}"))
        .await?;
    if args.preview {
        ctx.out.either(&preview, |_| print_preview(ctx, &preview))?;
        return Ok(0);
    }
    if !ctx.out.json {
        print_preview(ctx, &preview);
    }
    if let Some(why) = preview["mainBlocker"].as_str() {
        bail!("{why}");
    }
    if let Some(w) = preview["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|w| w["blocking"] == true)
    {
        bail!("{}", s(w, "message"));
    }
    let where_to = match &args.to {
        Some(t) => t.clone(),
        None => format!(
            "a new {} in {}",
            s(&target, "serverType"),
            s(&target, "location")
        ),
    };
    ctx.confirm(&format!(
        "Move everything on {} to {where_to}?",
        s(&preview, "name")
    ))?;
    let mut body = json!({ "target": target, "dnsSkip": args.skip_dns });
    if let Some(w) = workspace {
        body["workspaceId"] = json!(w);
    }
    let started: Value = api
        .post(&format!("/api/nodes/{node}/migrate"), &body)
        .await?;
    let id = s(&started, "id").to_owned();
    if args.detach {
        ctx.out.either(&started, |o| {
            o.line(format!("Started migration {id}"));
        })?;
        return Ok(0);
    }
    follow(ctx, &id, &node).await
}

/// Poll a migration, printing each new step, until it settles. Exit code 1
/// when it fails.
async fn follow(ctx: &Ctx, id: &str, node: &str) -> anyhow::Result<i32> {
    let api = ctx.api()?;
    ctx.out
        .note("Following the migration; Ctrl-C stops watching, not the migration.");
    let mut seen = HashSet::new();
    loop {
        let m: Value = api.get(&format!("/api/server-migrations/{id}")).await?;
        let moves = m["moves"].as_array().cloned().unwrap_or_default();
        let lines = m["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|st| format!("{} ({})", s(st, "label"), s(st, "status")))
            .chain(
                moves
                    .iter()
                    .map(|mv| format!("  {}: {}", s(mv, "service"), s(mv, "status"))),
            );
        for line in lines {
            if seen.insert(line.clone()) {
                ctx.out.note(&line);
            }
        }
        match s(&m, "status") {
            "done" => {
                ctx.out.either(&m, |o| {
                    o.line(format!(
                        "Done. {} now runs on {}. Delete the old server when you're happy.",
                        s(&m, "sourceName"),
                        s(&m, "targetName"),
                    ));
                })?;
                return Ok(0);
            }
            "aborted" => {
                ctx.out.either(&m, |o| {
                    o.line("Aborted; everything is back on the old server.");
                })?;
                return Ok(1);
            }
            "failed" => {
                let why = s(&m, "error").to_owned();
                ctx.out.either(&m, |o| {
                    o.line(format!("Failed: {why}"));
                    if s(&m, "step") == "dns" {
                        o.line(format!(
                            "Everything runs on the new server. Fix it, then: modula server migrate {node} --retry-dns"
                        ));
                    } else {
                        o.line("The old server is intact; abort it from the dashboard to move services back.");
                    }
                })?;
                return Ok(1);
            }
            _ => tokio::time::sleep(POLL).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ServerMigrateArgs {
        ServerMigrateArgs {
            server: "w1".into(),
            to: None,
            location: Some("aws:us-east-1".into()),
            server_type: Some("t3.small".into()),
            disk_gb: Some(120),
            workspace: None,
            skip_dns: Vec::new(),
            preview: false,
            detach: false,
            retry_dns: false,
        }
    }

    #[test]
    fn the_provider_comes_from_the_location() {
        let t = target(&args(), None).unwrap();
        assert_eq!(t["provider"], "aws");
        assert_eq!(t["diskGb"], 120);
        assert_eq!(
            preview_query(&t, Some("ws")),
            "workspaceId=ws&location=aws%3Aus-east-1&serverType=t3.small&diskGb=120"
        );
    }

    #[test]
    fn an_existing_target_wins() {
        let t = target(&args(), Some("abc")).unwrap();
        assert_eq!(t, json!({ "kind": "existing", "nodeId": "abc" }));
    }

    #[test]
    fn a_location_needs_its_provider() {
        let mut a = args();
        a.location = Some("hel1".into());
        assert!(target(&a, None).is_err());
    }
}
