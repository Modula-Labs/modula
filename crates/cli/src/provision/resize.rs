//! `modula server resize`: change the logged-in instance's main server to
//! another type at its cloud provider, from here. The control plane runs on that
//! server and can't power itself off and carry on, so the dashboard refuses
//! and this command does the steps: shutdown (power cut after
//! [`SHUTDOWN_GRACE`]) → change type (disk kept, so it can be downsized
//! again) → power on → wait for the dashboard's `/api/health`. Afterwards
//! the instance is asked for its resize options once more, which records the
//! new type.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use inquire::{Confirm, Password, PasswordDisplayMode, Select};
use modula_provider::{ActionState, Phase, Power, ProviderServer, ServerProvider, ServerTypeOffer};
use secrecy::SecretString;
use serde_json::{Value, json};

use super::catalog::format_usd;
use super::cloud::{self, Cloud, CloudSecret};
use super::flow::Settings;
use super::{ENV_AWS_KEY_ID, ENV_AWS_SECRET, ENV_HCLOUD, EnvSecrets, interactive};
use crate::api::{Api, api_error};
use crate::cli::{Provider, ServerResizeArgs};
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
    fn label(&self, current_cents: Option<i64>, approx: bool) -> String {
        let price = self
            .monthly_cents
            .map(|c| format!("{}/mo", format_usd(c, approx)))
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

/// Every type on offer, flagged for `server` at the catalog location
/// `location` (shown as `region`), usable ones first and cheapest first.
pub fn choices(
    provider: &dyn ServerProvider,
    server: &ProviderServer,
    location: &str,
    region: &str,
    types: &[ServerTypeOffer],
) -> Vec<Choice> {
    let mut out: Vec<Choice> = types
        .iter()
        .filter(|t| t.name != server.server_type)
        .map(|t| {
            let monthly_cents = t.price_in(location);
            let blocker = if monthly_cents.is_none() {
                Some(format!("not sold in {region} now"))
            } else {
                provider.resize_blocker(server, t)
            };
            Choice {
                name: t.name.clone(),
                cores: t.cores,
                memory_gb: t.memory_gb,
                disk_gb: t.disk_gb,
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
    let known = options.as_ref().filter(|v| v["managed"] == true);
    let kind = args
        .provider
        .unwrap_or(match known.map(|v| &v["provider"]) {
            Some(v) if v == "aws" => Provider::Aws,
            _ => Provider::Hetzner,
        });
    let known_id = known
        .filter(|v| v["provider"] == kind.id())
        .and_then(|v| v["providerId"].as_str())
        .map(str::to_owned);

    let secret = match env.secret_for(kind) {
        Some(s) => s,
        None if prompt => ask_credentials(kind)?,
        None => {
            return Err(usage(match kind {
                Provider::Hetzner => format!(
                    "set {ENV_HCLOUD} (or pass --token-stdin): the Hetzner token of the project the main server is in"
                ),
                Provider::Aws => format!(
                    "set {ENV_AWS_KEY_ID} and {ENV_AWS_SECRET}: an access key of the account the main server is in"
                ),
            }));
        }
    };
    let cloud = Cloud::connect(&secret, settings, "cli")?;
    let provider = cloud.provider();
    let e = move |err| cloud::error(kind, err);
    let server = match known_id {
        Some(id) => provider
            .describe(&id)
            .await
            .map_err(e)?
            .with_context(|| format!("{} no longer has this server", kind.label()))?,
        None => find_main(&cloud, &instance, main["publicIp"].as_str()).await?,
    };
    let place = server
        .location
        .with_context(|| format!("{} didn't say where the server is", kind.label()))?;
    let location = place.region.to_owned();
    let approx = kind.approximate_prices();
    let types = provider.server_types().await.map_err(e)?;
    let current_cents = types
        .iter()
        .find(|t| t.name == server.server_type)
        .and_then(|t| t.price_in(place.id));
    let all = choices(provider, &server, place.id, &location, &types);

    let target = match &args.server_type {
        Some(wanted) => {
            let Some(c) = all.iter().find(|c| c.name.eq_ignore_ascii_case(wanted)) else {
                if wanted.eq_ignore_ascii_case(&server.server_type) {
                    return Err(usage(format!(
                        "{} already is a {}",
                        server.name, server.server_type
                    )));
                }
                return Err(usage(format!(
                    "--type {wanted}: no such server type at {}. Possible here: {}",
                    kind.label(),
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
                o.accent(&server.server_type),
                server.cores,
                trim_float(server.memory_gb),
                server.disk_gb,
                current_cents
                    .map(|c| format!(", {}/mo", format_usd(c, approx)))
                    .unwrap_or_default()
            ));
            let usable: Vec<&Choice> = all.iter().filter(|c| c.blocker.is_none()).collect();
            if usable.is_empty() {
                bail!(
                    "no other server type can be used for {} in {location} now",
                    server.name
                );
            }
            struct Item<'a>(&'a Choice, Option<i64>, bool);
            impl std::fmt::Display for Item<'_> {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str(&self.0.label(self.1, self.2))
                }
            }
            inquire::set_global_render_config(super::wizard::theme());
            let items: Vec<Item> = usable
                .iter()
                .map(|c| Item(c, current_cents, approx))
                .collect();
            match Select::new("New server type", items)
                .with_help_message("the disk keeps its size · Esc: cancel")
                .with_page_size(12)
                .prompt()
            {
                Ok(Item(c, ..)) => c.clone(),
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
        server.ipv4.clone().unwrap_or_default(),
        server.server_type,
        target.name,
        match (current_cents, target.monthly_cents) {
            (Some(a), Some(b)) => format!(
                "{}/mo → {}/mo.",
                format_usd(a, approx),
                format_usd(b, approx)
            ),
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
    let result = resize(ctx, provider, kind, &server, &target.name, settings.poll).await;
    if let Err(e) = result {
        // Leave it running, at whatever size it has now.
        let state = provider.describe(&server.id).await.ok().flatten();
        if state.as_ref().is_some_and(|s| s.phase == Phase::Off) {
            let _ = provider.power(&server.id, Power::On).await;
        }
        return Err(e.context(format!(
            "resizing {} failed; it was started again{}",
            server.name,
            state
                .map(|s| format!(" as a {}", s.server_type))
                .unwrap_or_default()
        )));
    }

    step(ctx, "Waiting for the dashboard to answer…");
    wait_for_health(api, settings.poll).await?;
    // Has the instance record the new size (it reads it from the provider).
    let _ = api
        .get::<Value>(&format!("/api/nodes/{node_id}/resize-options"))
        .await;
    o.either(
        &json!({
            "server": server.name,
            "provider": kind.id(),
            "providerId": server.id,
            "from": server.server_type,
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

fn ask_credentials(kind: Provider) -> anyhow::Result<CloudSecret> {
    inquire::set_global_render_config(super::wizard::theme());
    if kind == Provider::Aws {
        return super::wizard::ask_aws_keys()?
            .map(CloudSecret::Aws)
            .context("no AWS access key given");
    }
    loop {
        let t = Password::new("Hetzner API token")
            .with_display_mode(PasswordDisplayMode::Masked)
            .without_confirmation()
            .with_help_message("Read & Write, from the project the main server is in")
            .prompt()?;
        if !t.trim().is_empty() {
            return Ok(CloudSecret::Hetzner(SecretString::from(
                t.trim().to_owned(),
            )));
        }
    }
}

/// The IPv4s the instance's host resolves to.
async fn host_addresses(instance: &url::Url) -> Vec<String> {
    let host = instance.host_str().unwrap_or_default().to_owned();
    tokio::net::lookup_host((host.as_str(), 443))
        .await
        .map(|a| a.map(|a| a.ip().to_string()).collect())
        .unwrap_or_default()
}

/// The main server when the instance couldn't name it. On Hetzner the one
/// labelled `role=main`, or among several the one the instance's host
/// resolves to; elsewhere the Modula server at the dashboard node's address
/// or the host's.
async fn find_main(
    cloud: &Cloud,
    instance: &url::Url,
    node_ip: Option<&str>,
) -> anyhow::Result<ProviderServer> {
    let kind = cloud.kind();
    if let Cloud::Hetzner(h) = cloud {
        let mut mains = h.main_servers().await.map_err(|e| cloud::error(kind, e))?;
        if mains.len() > 1 {
            let ips = host_addresses(instance).await;
            mains.retain(|s| s.ipv4.as_ref().is_some_and(|ip| ips.contains(ip)));
        }
        return match mains.len() {
            1 => Ok(mains.remove(0)),
            0 => bail!(
                "this Hetzner project has no Modula main server (labelled role=main) for {instance}; is it the right token?"
            ),
            _ => bail!("several Modula main servers in this Hetzner project match {instance}"),
        };
    }
    let mut ips: Vec<String> = node_ip.map(str::to_owned).into_iter().collect();
    ips.extend(host_addresses(instance).await);
    for ip in &ips {
        if let Some(server) = cloud
            .provider()
            .find_by_address(ip)
            .await
            .map_err(|e| cloud::error(kind, e))?
        {
            return Ok(server);
        }
    }
    bail!(
        "no Modula server at {} matches {instance}; is it the right account?",
        kind.label()
    )
}

/// Wait until the server reaches `want`, polling the provider.
async fn wait_phase(
    provider: &dyn ServerProvider,
    kind: Provider,
    id: &str,
    poll: Duration,
    within: Duration,
    want: Phase,
) -> anyhow::Result<bool> {
    let started = Instant::now();
    loop {
        let s = provider
            .status(id, None)
            .await
            .map_err(|e| cloud::error(kind, e))?;
        if s.phase == want {
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
    provider: &dyn ServerProvider,
    kind: Provider,
    server: &ProviderServer,
    to: &str,
    poll: Duration,
) -> anyhow::Result<()> {
    let id = server.id.as_str();
    let e = move |err| cloud::error(kind, err);
    if server.phase != Phase::Off {
        step(ctx, "Shutting the server down…");
        provider.power(id, Power::Off).await.map_err(e)?;
        if !wait_phase(provider, kind, id, poll, SHUTDOWN_GRACE, Phase::Off).await? {
            step(ctx, "It didn't stop in time; cutting the power…");
            provider.power(id, Power::ForceOff).await.map_err(e)?;
            if !wait_phase(provider, kind, id, poll, POWEROFF_WAIT, Phase::Off).await? {
                bail!("the server did not power off");
            }
        }
    }

    step(
        ctx,
        &format!("Changing the type to {to} (the disk keeps its size)…"),
    );
    let action = provider.resize(id, to).await.map_err(e)?;
    let started = Instant::now();
    loop {
        match provider.action(&action).await.map_err(e)? {
            ActionState::Done => break,
            ActionState::Failed(why) => {
                bail!("{} couldn't change the type: {why}", kind.label())
            }
            ActionState::Running if started.elapsed() > CHANGE_TYPE_WAIT => {
                bail!("{} took too long to change the type", kind.label())
            }
            ActionState::Running => tokio::time::sleep(poll).await,
        }
    }

    step(ctx, "Starting it again…");
    let state = provider.status(id, None).await.map_err(e)?;
    if state.phase != Phase::Running {
        provider.power(id, Power::On).await.map_err(e)?;
    }
    if !wait_phase(provider, kind, id, poll, POWERON_WAIT, Phase::Running).await? {
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
    use modula_provider::{LocationPrice, locations};

    fn offer(name: &str, disk: u32, arch: &str, prices: &[(&'static str, i64)]) -> ServerTypeOffer {
        ServerTypeOffer {
            name: name.into(),
            description: String::new(),
            cores: 4,
            memory_gb: 8.0,
            disk_gb: disk,
            arch: arch.into(),
            cpu_type: "shared".into(),
            storage_type: "local".into(),
            prices: prices
                .iter()
                .map(|(location, monthly_cents)| LocationPrice {
                    location,
                    monthly_cents: *monthly_cents,
                })
                .collect(),
        }
    }

    #[test]
    fn choices_flag_what_cant_be_used_here() {
        let hz = cloud::HetznerCloud::new(
            modula_hetzner::Client::new(
                "http://127.0.0.1:9/v1".parse().unwrap(),
                "t".to_owned().into(),
            )
            .unwrap(),
        );
        let server = ProviderServer {
            id: "1".into(),
            name: "modula-main".into(),
            ipv4: None,
            phase: Phase::Running,
            location: locations::resolve("hetzner", "fsn1", ""),
            server_type: "cx32".into(),
            cores: 4,
            memory_gb: 8.0,
            disk_gb: 80,
            arch: "x86".into(),
            storage_type: "local".into(),
            role: Some("main".into()),
        };
        let f = "hetzner:fsn1";
        let types = [
            offer("cx22", 40, "x86", &[(f, 487)]),
            offer("cx32", 80, "x86", &[(f, 887)]),
            offer("cx42", 160, "x86", &[(f, 2043)]),
            offer("cx52", 320, "x86", &[("hetzner:nbg1", 3928)]),
            offer("cax21", 80, "arm", &[(f, 809)]),
        ];
        let c = choices(&hz, &server, f, "fsn1", &types);
        let by = |n: &str| c.iter().find(|c| c.name == n).unwrap();
        assert!(c.iter().all(|c| c.name != "cx32"), "not the current type");
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
        assert_eq!(by("cx42").monthly_cents, Some(2043));
        assert_eq!(possible(&c), "cx42", "bigger, same architecture, sold here");
        assert_eq!(c[0].name, "cx42", "usable ones first");
    }
}
