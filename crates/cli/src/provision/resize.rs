//! `modula server resize`: change the logged-in instance's main server to
//! another type at Hetzner, from here. The control plane runs on that
//! server and can't power itself off and carry on, so the dashboard refuses
//! and this command does the steps: shutdown (power cut after
//! [`SHUTDOWN_GRACE`]) → change type (disk kept, so it can be downsized
//! again) → power on → wait for the dashboard's `/api/health`. Afterwards
//! the instance is asked for its resize options once more, which records the
//! new type.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use inquire::{Confirm, Password, PasswordDisplayMode, Select};
use modula_hetzner::{Client, Server, ServerType};
use secrecy::SecretString;
use serde_json::{Value, json};

use super::catalog::format_eur;
use super::flow::{self, Settings};
use super::{ENV_HCLOUD, EnvSecrets, interactive};
use crate::api::{Api, api_error};
use crate::cli::ServerResizeArgs;
use crate::context::Ctx;
use crate::output::usage;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(90);
const POWEROFF_WAIT: Duration = Duration::from_secs(120);
const CHANGE_TYPE_WAIT: Duration = Duration::from_secs(30 * 60);
const POWERON_WAIT: Duration = Duration::from_secs(5 * 60);
const HEALTH_WAIT: Duration = Duration::from_secs(10 * 60);

/// A type the server could change to here, with why not when it can't.
#[derive(Debug, Clone)]
pub struct Choice {
    pub name: String,
    pub cores: u32,
    pub memory_gb: f64,
    pub disk_gb: u32,
    pub monthly_cents: Option<i64>,
    /// Why it can't be chosen (not sold here, architecture, disk).
    pub blocker: Option<String>,
}

impl Choice {
    fn label(&self, current_cents: Option<i64>) -> String {
        let price = self
            .monthly_cents
            .map(|c| format!("{}/mo", format_eur(c)))
            .unwrap_or_default();
        let trend = match (self.monthly_cents, current_cents) {
            (Some(a), Some(b)) if a > b => "  ↑",
            (Some(a), Some(b)) if a < b => "  ↓",
            _ => "",
        };
        format!(
            "{:<8} {:>2} vCPU · {:>4} GB RAM · {:>4} GB disk · {price}{trend}",
            self.name,
            self.cores,
            trim_float(self.memory_gb),
            self.disk_gb
        )
    }
}

fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

/// Every orderable type, flagged for `server` in `location` (cheapest first).
pub fn choices(server: &Server, location: &str, types: &[ServerType]) -> Vec<Choice> {
    let current = server.server_type.as_ref().map(|t| t.name.as_str());
    let mut out: Vec<Choice> = types
        .iter()
        .filter(|t| !t.is_deprecated() && Some(t.name.as_str()) != current)
        .map(|t| {
            let monthly_cents = t
                .orderable_in(location)
                .then(|| t.monthly_price_in(location))
                .flatten()
                .and_then(|a| modula_hetzner::price_cents(&a.gross));
            let blocker = if monthly_cents.is_none() {
                Some(format!("not sold in {location} now"))
            } else {
                server.change_type_blocker(t)
            };
            Choice {
                name: t.name.clone(),
                cores: t.cores,
                memory_gb: t.memory,
                disk_gb: t.disk,
                monthly_cents,
                blocker,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (
            a.blocker.is_some(),
            a.monthly_cents.unwrap_or(i64::MAX),
            &a.name,
        )
            .cmp(&(
                b.blocker.is_some(),
                b.monthly_cents.unwrap_or(i64::MAX),
                &b.name,
            ))
    });
    out
}

/// `modula server resize [--type T]`.
pub async fn run(
    ctx: &Ctx,
    args: &ServerResizeArgs,
    env: EnvSecrets,
    settings: &Settings,
) -> anyhow::Result<i32> {
    let o = &ctx.out;
    let prompt = interactive(ctx);
    let api = ctx.api()?;
    let instance = api.login.url.clone();

    // The node running the dashboard, and what the instance knows about it.
    let nodes: Vec<Value> = api.get("/api/nodes").await.map_err(|e| {
        if api_error(&e).is_some_and(|a| a.status == 403 || a.status == 404) {
            anyhow::anyhow!("resizing the main server needs an instance admin's login")
        } else {
            e
        }
    })?;
    let main = nodes
        .iter()
        .find(|n| {
            n["info"]["capabilities"]
                .as_array()
                .is_some_and(|c| c.iter().any(|v| v == "dashboard"))
        })
        .with_context(|| format!("{instance} lists no server running the dashboard"))?;
    let node_id = main["id"].as_str().unwrap_or_default().to_owned();
    let options: Option<Value> = api
        .get(&format!("/api/nodes/{node_id}/resize-options"))
        .await
        .ok();
    let known_id = options
        .as_ref()
        .filter(|v| v["managed"] == true && v["provider"] == "hetzner")
        .and_then(|v| v["providerId"].as_str())
        .and_then(|v| v.parse::<i64>().ok());

    let token = match env.hcloud {
        Some(t) => t,
        None if prompt => ask_token()?,
        None => {
            return Err(usage(format!(
                "set {ENV_HCLOUD} (or pass --token-stdin): the Hetzner token of the project the main server is in"
            )));
        }
    };
    let hz = flow::hetzner(settings, &token)?;
    let server = match known_id {
        Some(id) => hz.server(id).await.map_err(flow::hetzner_error)?,
        None => find_main(&hz, &instance).await?,
    };
    let location = server
        .location
        .as_ref()
        .map(|l| l.name.clone())
        .context("Hetzner didn't say where the server is")?;
    let current = server
        .server_type
        .clone()
        .context("Hetzner didn't say the server's type")?;
    let current_cents = current
        .monthly_price_in(&location)
        .and_then(|a| modula_hetzner::price_cents(&a.gross));
    let types = hz.server_types().await.map_err(flow::hetzner_error)?;
    let all = choices(&server, &location, &types);

    let target = match &args.server_type {
        Some(wanted) => {
            let Some(c) = all.iter().find(|c| c.name.eq_ignore_ascii_case(wanted)) else {
                if wanted.eq_ignore_ascii_case(&current.name) {
                    return Err(usage(format!(
                        "{} already is a {}",
                        server.name, current.name
                    )));
                }
                return Err(usage(format!(
                    "--type {wanted}: no such server type at Hetzner. Possible here: {}",
                    possible(&all)
                )));
            };
            if let Some(why) = &c.blocker {
                return Err(usage(format!(
                    "--type {}: {why}. Possible here: {}",
                    c.name,
                    possible(&all)
                )));
            }
            c.clone()
        }
        None if prompt => {
            o.note(format!(
                "  {} is a {} in {location}: {} vCPU, {} GB RAM, {} GB disk{}.",
                server.name,
                o.accent(&current.name),
                current.cores,
                trim_float(current.memory),
                server.disk_gb().unwrap_or(current.disk),
                current_cents
                    .map(|c| format!(", {}/mo", format_eur(c)))
                    .unwrap_or_default()
            ));
            let usable: Vec<&Choice> = all.iter().filter(|c| c.blocker.is_none()).collect();
            if usable.is_empty() {
                bail!(
                    "no other server type can be used for {} in {location} now",
                    server.name
                );
            }
            struct Item<'a>(&'a Choice, Option<i64>);
            impl std::fmt::Display for Item<'_> {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str(&self.0.label(self.1))
                }
            }
            inquire::set_global_render_config(super::wizard::theme());
            let items: Vec<Item> = usable.iter().map(|c| Item(c, current_cents)).collect();
            match Select::new("New server type", items)
                .with_help_message("the disk keeps its size · Esc: cancel")
                .with_page_size(12)
                .prompt()
            {
                Ok(Item(c, _)) => c.clone(),
                Err(
                    inquire::InquireError::OperationCanceled
                    | inquire::InquireError::OperationInterrupted,
                ) => {
                    o.note("  Okay, nothing was changed.");
                    return Ok(1);
                }
                Err(e) => return Err(e.into()),
            }
        }
        None => {
            return Err(usage(format!(
                "--type is required without a terminal. Possible here: {}",
                possible(&all)
            )));
        }
    };

    let warning = format!(
        "{} ({}) will be shut down, changed from {} to {} and started again: the dashboard and \
         everything on this server are down for a few minutes. {}",
        server.name,
        server.ipv4().unwrap_or_default(),
        current.name,
        target.name,
        match (current_cents, target.monthly_cents) {
            (Some(a), Some(b)) => format!("{}/mo → {}/mo.", format_eur(a), format_eur(b)),
            _ => String::new(),
        }
    );
    if !ctx.global.yes {
        if !prompt {
            return Err(usage(format!("{warning} Pass --yes to confirm.")));
        }
        o.note(format!("  {warning}"));
        let ok = Confirm::new("Resize now?")
            .with_default(false)
            .prompt()
            .unwrap_or(false);
        if !ok {
            o.note("  Okay, nothing was changed.");
            return Ok(1);
        }
    }

    let started = Instant::now();
    let result = resize(ctx, &hz, &server, &target.name, settings.poll).await;
    if let Err(e) = result {
        // Leave it running, at whatever size it has now.
        let state = hz.server(server.id).await.ok();
        if state.as_ref().is_some_and(|s| s.status == "off") {
            let _ = hz.server_action(server.id, "poweron").await;
        }
        return Err(e.context(format!(
            "resizing {} failed; it was started again{}",
            server.name,
            state
                .and_then(|s| s.server_type.map(|t| format!(" as a {}", t.name)))
                .unwrap_or_default()
        )));
    }

    step(ctx, "Waiting for the dashboard to answer…");
    wait_for_health(api, settings.poll).await?;
    // Has the instance record the new size (it reads it from Hetzner).
    let _ = api
        .get::<Value>(&format!("/api/nodes/{node_id}/resize-options"))
        .await;
    o.either(
        &json!({
            "server": server.name,
            "providerId": server.id.to_string(),
            "from": current.name,
            "to": target.name,
            "monthlyCents": target.monthly_cents,
            "elapsedSecs": started.elapsed().as_secs(),
        }),
        |o| {
            o.line(format!(
                "✓ {} is a {} now ({}s); {} answers again.",
                server.name,
                o.accent(&target.name),
                started.elapsed().as_secs(),
                instance.as_str().trim_end_matches('/')
            ));
        },
    )?;
    Ok(0)
}

fn possible(all: &[Choice]) -> String {
    let names: Vec<&str> = all
        .iter()
        .filter(|c| c.blocker.is_none())
        .map(|c| c.name.as_str())
        .collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

fn step(ctx: &Ctx, text: &str) {
    if !ctx.out.json {
        ctx.out.note(format!("  {}", ctx.out.dim(text)));
    }
}

fn ask_token() -> anyhow::Result<SecretString> {
    inquire::set_global_render_config(super::wizard::theme());
    loop {
        let t = Password::new("Hetzner API token")
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .with_help_message("Read & Write, from the project the main server is in")
            .prompt()?;
        if !t.trim().is_empty() {
            return Ok(SecretString::from(t.trim().to_owned()));
        }
    }
}

/// The main server when the instance couldn't name it: the one labelled
/// `role=main`, or among several the one the instance's host resolves to.
async fn find_main(hz: &Client, instance: &url::Url) -> anyhow::Result<Server> {
    let mut mains: Vec<Server> = hz
        .servers("modula=1")
        .await
        .map_err(flow::hetzner_error)?
        .into_iter()
        .filter(|s| s.labels.get("role").map(String::as_str) == Some("main"))
        .collect();
    if mains.len() > 1 {
        let host = instance.host_str().unwrap_or_default().to_owned();
        let ips: Vec<String> = tokio::net::lookup_host((host.as_str(), 443))
            .await
            .map(|a| a.map(|a| a.ip().to_string()).collect())
            .unwrap_or_default();
        mains.retain(|s| s.ipv4().is_some_and(|ip| ips.contains(&ip)));
    }
    match mains.len() {
        1 => Ok(mains.remove(0)),
        0 => bail!(
            "this Hetzner project has no Modula main server (labelled role=main) for {instance}; is it the right token?"
        ),
        _ => bail!("several Modula main servers in this Hetzner project match {instance}"),
    }
}

/// Wait until `cond` holds for the server, polling Hetzner.
async fn wait_server(
    hz: &Client,
    id: i64,
    poll: Duration,
    within: Duration,
    cond: impl Fn(&Server) -> bool,
) -> anyhow::Result<bool> {
    let started = Instant::now();
    loop {
        let s = hz.server(id).await.map_err(flow::hetzner_error)?;
        if cond(&s) {
            return Ok(true);
        }
        if started.elapsed() > within {
            return Ok(false);
        }
        tokio::time::sleep(poll).await;
    }
}

async fn resize(
    ctx: &Ctx,
    hz: &Client,
    server: &Server,
    to: &str,
    poll: Duration,
) -> anyhow::Result<()> {
    let id = server.id;
    if server.status != "off" {
        step(ctx, "Shutting the server down…");
        hz.server_action(id, "shutdown")
            .await
            .map_err(flow::hetzner_error)?;
        let off = |s: &Server| s.status == "off";
        if !wait_server(hz, id, poll, SHUTDOWN_GRACE, off).await? {
            step(ctx, "It didn't stop in time; cutting the power…");
            hz.server_action(id, "poweroff")
                .await
                .map_err(flow::hetzner_error)?;
            if !wait_server(hz, id, poll, POWEROFF_WAIT, off).await? {
                bail!("the server did not power off");
            }
        }
    }

    step(
        ctx,
        &format!("Changing the type to {to} (the disk keeps its size)…"),
    );
    let action = hz
        .change_server_type(id, to, false)
        .await
        .map_err(flow::hetzner_error)?;
    let started = Instant::now();
    loop {
        let a = hz.action(action.id).await.map_err(flow::hetzner_error)?;
        match a.status.as_str() {
            "success" => break,
            "error" => bail!(
                "Hetzner couldn't change the type: {}",
                a.error
                    .map(|e| format!("{} ({})", e.message, e.code))
                    .unwrap_or_else(|| "unknown error".into())
            ),
            _ if started.elapsed() > CHANGE_TYPE_WAIT => {
                bail!("Hetzner took too long to change the type")
            }
            _ => tokio::time::sleep(poll).await,
        }
    }

    step(ctx, "Starting it again…");
    let s = hz.server(id).await.map_err(flow::hetzner_error)?;
    if s.status != "running" {
        hz.server_action(id, "poweron")
            .await
            .map_err(flow::hetzner_error)?;
    }
    if !wait_server(hz, id, poll, POWERON_WAIT, |s| s.status == "running").await? {
        bail!("the server did not start again");
    }
    Ok(())
}

async fn wait_for_health(api: &Api, poll: Duration) -> anyhow::Result<()> {
    let started = Instant::now();
    loop {
        match api.get::<Value>("/api/health").await {
            Ok(_) => return Ok(()),
            Err(e) if started.elapsed() > HEALTH_WAIT => {
                return Err(e).context(
                    "the server runs at its new size, but the dashboard doesn't answer yet; check it over SSH (systemctl status modula-server)",
                );
            }
            Err(_) => tokio::time::sleep(poll).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> Vec<ServerType> {
        let price = |loc: &str, gross: &str| json!({"location": loc, "price_monthly": {"net": "1", "gross": gross}});
        serde_json::from_value(json!([
            {"name": "cx22", "cores": 2, "memory": 4.0, "disk": 40, "architecture": "x86", "storage_type": "local",
             "prices": [price("fsn1", "4.51")]},
            {"name": "cx32", "cores": 4, "memory": 8.0, "disk": 80, "architecture": "x86", "storage_type": "local",
             "prices": [price("fsn1", "8.21")]},
            {"name": "cx42", "cores": 8, "memory": 16.0, "disk": 160, "architecture": "x86", "storage_type": "local",
             "prices": [price("fsn1", "18.92")]},
            {"name": "cx52", "cores": 16, "memory": 32.0, "disk": 320, "architecture": "x86", "storage_type": "local",
             "prices": [price("nbg1", "36.37")]},
            {"name": "cax21", "cores": 4, "memory": 8.0, "disk": 80, "architecture": "arm", "storage_type": "local",
             "prices": [price("fsn1", "7.49")]}
        ]))
        .unwrap()
    }

    #[test]
    fn choices_flag_what_cant_be_used_here() {
        let server: Server = serde_json::from_value(json!({
            "id": 1, "name": "modula-main", "status": "running",
            "server_type": {"name": "cx32", "cores": 4, "memory": 8.0, "disk": 80,
                            "architecture": "x86", "storage_type": "local"},
            "location": {"name": "fsn1"}, "primary_disk_size": 80
        }))
        .unwrap();
        let c = choices(&server, "fsn1", &types());
        let by = |n: &str| c.iter().find(|c| c.name == n).unwrap();
        assert!(
            c.iter().all(|c| c.name != "cx32"),
            "the current type isn't offered"
        );
        assert!(
            by("cx22")
                .blocker
                .as_ref()
                .unwrap()
                .contains("can't shrink")
        );
        assert!(
            by("cx52")
                .blocker
                .as_ref()
                .unwrap()
                .contains("not sold in fsn1")
        );
        assert!(
            by("cax21")
                .blocker
                .as_ref()
                .unwrap()
                .contains("architectures")
        );
        assert_eq!(by("cx42").monthly_cents, Some(1892));
        assert_eq!(possible(&c), "cx42", "bigger, same architecture, sold here");
        assert_eq!(c[0].name, "cx42", "usable ones first");
    }
}
