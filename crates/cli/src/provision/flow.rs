//! Creating the server once everything is decided ([`Plan`]): firewall,
//! SSH key, server, DNS, waiting for the install, then setup and login.
//! What was created is recorded as it happens ([`Created`]) so a failure at
//! any point can delete exactly that, and nothing keeps billing.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use modula_provider::{ActionState, NewServer, Phase as CloudPhase, ProviderError};
use secrecy::{ExposeSecret, SecretString};
use url::Url;

use super::catalog::{LocationOption, TypeOption};
use super::cloud::{self, Cloud, CloudSecret};
use super::cloud_init::MainBootstrap;
use super::cloudflare::{self, DnsRecord, Zone};
use super::dns::{self, Pointing, Resolver};
use super::install_progress::{self, InstallError, Report, Reporter, friendly};
use super::instance::Instance;
use super::progress::{Event, Progress, duration};
use super::setup_token;
use crate::cli::Provider;

pub const IMAGE: &str = "ubuntu-24.04";
/// How long a new server may take to get its public address.
const ADDRESS_WAIT: Duration = Duration::from_secs(10 * 60);

/// Everything `server create` needs, decided and checked (wizard or flags).
#[derive(Debug, Clone)]
pub struct Plan {
    pub provider: Provider,
    pub location: LocationOption,
    pub server_type: TypeOption,
    pub name: String,
    pub domain: String,
    pub email: String,
    pub ssh: SshChoice,
    pub dns: DnsPlan,
    pub install_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SshChoice {
    /// A key already at the provider.
    Existing { id: String, name: String },
    /// A local public key to upload first.
    Upload {
        name: String,
        public_key: String,
        path: PathBuf,
    },
    /// No key: Hetzner emails a root password; on AWS nobody can log in.
    None,
}

#[derive(Debug, Clone)]
pub enum DnsPlan {
    /// The user adds the A record; we wait for it.
    Manual,
    /// No domain yet: the dashboard is served at `https://<ip>` (an IP
    /// certificate); a domain is added later in the dashboard.
    /// `Plan::domain` is empty until the server has its IP.
    Instant,
    /// We create it, replacing these records first.
    Cloudflare { zone: Zone, replace: Vec<DnsRecord> },
}

/// Where to talk to and how long to wait (tests shorten and redirect it).
pub struct Settings {
    pub hetzner_api: Url,
    /// Where EC2 requests go instead of AWS (tests).
    pub aws_api: Option<Url>,
    pub cloudflare_api: Url,
    /// Send requests for the new instance here instead of `https://<domain>`
    /// pinned to the server's IP (tests).
    pub instance_via: Option<Url>,
    /// Fetch the installer's progress from here instead of `http://<ip>/`
    /// (tests).
    pub progress_via: Option<Url>,
    pub dns: Box<dyn Resolver>,
    pub poll: Duration,
    pub dns_timeout: Duration,
    pub install_timeout: Duration,
}

impl Settings {
    pub fn production(dns_timeout: Duration, install_timeout: Duration) -> Self {
        Self {
            hetzner_api: modula_hetzner::API_URL.parse().expect("static url"),
            aws_api: None,
            cloudflare_api: cloudflare::API_URL.parse().expect("static url"),
            instance_via: None,
            progress_via: None,
            dns: Box::new(dns::PublicDns),
            poll: Duration::from_secs(3),
            dns_timeout,
            install_timeout,
        }
    }
}

pub struct Secrets {
    pub cloud: CloudSecret,
    pub cloudflare: Option<SecretString>,
}

/// What exists because of this run (and must go if it fails).
#[derive(Debug, Default, Clone)]
pub struct Created {
    /// Only when created by this run (a reused one stays).
    pub firewall: Option<String>,
    pub ssh_key: Option<String>,
    pub server: Option<String>,
    /// Labels the server; set just before asking for it, so an interrupted
    /// or failed create can still be found and cleaned up.
    pub modula_id: Option<String>,
    pub ip: Option<Ipv4Addr>,
    /// (zone id, record id).
    pub dns_record: Option<(String, String)>,
}

/// A failure with what to tell the user about it.
#[derive(Debug)]
pub struct Failure {
    pub error: anyhow::Error,
    pub hint: Option<String>,
}

impl<E: Into<anyhow::Error>> From<E> for Failure {
    fn from(e: E) -> Self {
        Self {
            error: e.into(),
            hint: None,
        }
    }
}

fn fail(error: anyhow::Error, hint: impl Into<String>) -> Failure {
    Failure {
        error,
        hint: Some(hint.into()),
    }
}

fn labels(plan: &Plan) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("modula".to_owned(), "1".to_owned()),
        ("role".to_owned(), "main".to_owned()),
        ("server".to_owned(), plan.name.clone()),
    ])
}

/// A provider's id as `--json` shows it: a number when it is one.
pub fn json_id(id: &str) -> serde_json::Value {
    id.parse::<i64>()
        .map_or_else(|_| serde_json::json!(id), |n| serde_json::json!(n))
}

/// A random `modula-id` label (fits Hetzner labels and EC2 client tokens).
fn new_modula_id() -> anyhow::Result<String> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(hex::encode(raw))
}

/// The dashboard's address for [`DnsPlan::Instant`]: the server's IP (the
/// installer serves it there when it gets no domain).
pub fn instant_domain(ip: Ipv4Addr) -> String {
    ip.to_string()
}

/// Create the server and wait until its dashboard answers over HTTPS.
/// Returns the IP; `created` says what exists, also on failure. With
/// [`DnsPlan::Instant`] it fills in `plan.domain` once the IP is known.
pub async fn provision(
    plan: &mut Plan,
    secrets: &Secrets,
    settings: &Settings,
    setup_token: &SecretString,
    progress: &mut Progress,
    created: &mut Created,
) -> Result<Ipv4Addr, Failure> {
    let cloud = Cloud::connect(&secrets.cloud, settings, &format!("cli-{}", plan.name))?;
    let kind = cloud.kind();
    let he = move |e: ProviderError| cloud::error(kind, e);
    let provider = cloud.provider();
    let labels = labels(plan);

    if cloud.firewall_before_server() {
        // Firewall: reuse ours from an earlier attempt with this name.
        progress.start("Creating the firewall");
        let firewall = cloud.prepare_firewall(&plan.name, &labels).await?;
        if !firewall.reused {
            created.firewall = Some(firewall.id.clone());
        }
        progress.done(if firewall.reused {
            "Firewall ready (reused)"
        } else {
            "Firewall created"
        });
        progress.event(&Event::FirewallCreated {
            id: json_id(&firewall.id),
            name: firewall.name,
            reused: firewall.reused,
        });
    }

    let ssh_keys = match &plan.ssh {
        SshChoice::Existing { id, .. } => vec![id.clone()],
        SshChoice::Upload {
            name, public_key, ..
        } => {
            progress.start("Uploading your SSH key");
            let key = cloud
                .upload_ssh_key(&plan.location.name, name, public_key)
                .await
                .map_err(he)?;
            created.ssh_key = Some(key.id.clone());
            progress.done(&format!("SSH key uploaded ({name})"));
            progress.event(&Event::SshKeyUploaded {
                id: json_id(&key.id),
                name: name.clone(),
            });
            vec![key.id]
        }
        SshChoice::None => {
            if kind == Provider::Aws {
                progress.note(
                    "No SSH key: AWS has no emailed root password, so nobody can log in to this server. Pass --ssh-key-file to be able to.",
                );
            }
            vec![]
        }
    };

    progress.start("Spinning up the server");
    let progress_token = install_progress::new_token()?;
    let user_data = MainBootstrap {
        install_url: &plan.install_url,
        domain: (!matches!(plan.dns, DnsPlan::Instant)).then_some(plan.domain.as_str()),
        email: &plan.email,
        name: &plan.name,
        setup_token: setup_token.expose_secret(),
        progress_token: &progress_token,
        key_only_ssh: !ssh_keys.is_empty(),
    }
    .render();
    let modula_id = new_modula_id()?;
    created.modula_id = Some(modula_id.clone());
    let mut server_labels = labels.clone();
    server_labels.insert("modula-id".to_owned(), modula_id);
    let req = NewServer {
        name: plan.name.clone(),
        region: plan.location.name.clone(),
        server_type: plan.server_type.name.clone(),
        user_data,
        labels: server_labels,
        ssh_key_ids: ssh_keys,
        ssh: true,
        disk_gb: None,
    };
    let server = provider.create(&req).await.map_err(|e| match e {
        ProviderError::Api { code, message } if code == "uniqueness_error" => fail(
            anyhow::anyhow!("{}: {message}", kind.label()),
            format!(
                "A server named {} already exists in this {} project; pick another --name.",
                plan.name,
                kind.label()
            ),
        ),
        other => he(other).into(),
    })?;
    drop(req);
    created.server = Some(server.id.clone());
    let ip: Ipv4Addr = match server.ipv4.as_deref().and_then(|ip| ip.parse().ok()) {
        Some(ip) => ip,
        None if kind == Provider::Hetzner => {
            return Err(anyhow::anyhow!(
                "Hetzner created the server without a public IPv4 address"
            )
            .into());
        }
        // EC2: its Elastic IP is attached once the instance runs.
        None => wait_for_address(provider, &server.id, settings, progress, kind).await?,
    };
    created.ip = Some(ip);
    provider
        .open_node_port(&server.id, &plan.name, cloud::NODE_PORT)
        .await
        .map_err(he)?;
    if !cloud.firewall_before_server() {
        progress.event(&Event::FirewallCreated {
            id: json_id(&format!("modula-main-{}", plan.name)),
            name: format!("modula-main-{}", plan.name),
            reused: false,
        });
    }
    if matches!(plan.dns, DnsPlan::Instant) {
        plan.domain = instant_domain(ip);
    }
    progress.done(&format!(
        "Server created: {} · {} · {ip}",
        plan.name, plan.server_type.name
    ));
    progress.event(&Event::ServerCreated {
        id: json_id(&server.id),
        name: plan.name.clone(),
        ip: ip.to_string(),
    });

    // DNS first, so it propagates while the server installs.
    match &plan.dns {
        DnsPlan::Cloudflare { zone, replace } => {
            progress.start(&format!("Pointing {} at {ip} (Cloudflare)", plan.domain));
            let token = secrets
                .cloudflare
                .clone()
                .context("no Cloudflare API token")?;
            let cf = cloudflare::Client::new(settings.cloudflare_api.clone(), token)?;
            for r in replace {
                cf.delete_record(&zone.id, &r.id).await?;
            }
            let record = cf.create_a(&zone.id, &plan.domain, &ip.to_string()).await?;
            created.dns_record = Some((zone.id.clone(), record.id));
            progress.done(&format!("DNS record created: {} → {ip}", plan.domain));
            progress.event(&Event::DnsRecordCreated {
                name: plan.domain.clone(),
                ip: ip.to_string(),
                zone: zone.name.clone(),
            });
        }
        DnsPlan::Instant => {
            progress.aside(&format!(
                "No domain yet: the dashboard is at https://{} (add a domain there later)",
                plan.domain
            ));
        }
        DnsPlan::Manual => {
            let answer = settings.dns.ipv4(&plan.domain).await.unwrap_or_default();
            if dns::pointing(&answer, ip) != Pointing::Here {
                let current: Vec<String> = answer.iter().map(ToString::to_string).collect();
                progress.event(&Event::DnsWaiting {
                    record_type: "A".into(),
                    name: plan.domain.clone(),
                    value: ip.to_string(),
                    current: current.clone(),
                });
                progress.note("");
                progress.note(&format!(
                    "Add this DNS record now (DNS only, not proxied): {}",
                    progress.out.accent(&format!("{}  A  {ip}", plan.domain))
                ));
                if !current.is_empty() {
                    progress.note(&format!(
                        "It points at {} now; replace that.",
                        current.join(", ")
                    ));
                }
                progress.note("The server installs meanwhile; the certificate waits for it.");
                progress.note("");
            }
        }
    }

    watch(
        plan,
        settings,
        &cloud,
        &server.id,
        server.action_id.clone(),
        ip,
        &progress_token,
        progress,
    )
    .await?;
    Ok(ip)
}

/// Poll until the provider reports the server's public address.
async fn wait_for_address(
    provider: &dyn modula_provider::ServerProvider,
    id: &str,
    settings: &Settings,
    progress: &mut Progress,
    kind: Provider,
) -> Result<Ipv4Addr, Failure> {
    let t = Instant::now();
    loop {
        let state = provider
            .status(id, None)
            .await
            .map_err(|e| Failure::from(cloud::error(kind, e)))?;
        if let Some(why) = state.error {
            return Err(anyhow::anyhow!("{}: {why}", kind.label()).into());
        }
        if state.phase == CloudPhase::Gone {
            return Err(anyhow::anyhow!("{} terminated the server", kind.label()).into());
        }
        if let Some(ip) = state.ipv4.as_deref().and_then(|ip| ip.parse().ok()) {
            progress.event(&Event::ServerRunning {
                id: json_id(id),
                elapsed_secs: t.elapsed().as_secs(),
            });
            return Ok(ip);
        }
        if t.elapsed() > ADDRESS_WAIT {
            return Err(anyhow::anyhow!(
                "the server didn't get a public address within {}",
                duration(ADDRESS_WAIT)
            )
            .into());
        }
        pause(
            progress,
            settings.poll,
            Some("waiting for its public address"),
        )
        .await;
    }
}

/// Where the install is, as far as the CLI can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Hetzner boots the server; cloud-init hasn't started install.sh yet.
    Booting,
    /// install.sh reports its steps.
    Installer,
    /// install.sh handed port 80 over: services start.
    Starting,
    /// Caddy answers but has no certificate yet.
    Certificate,
}

/// Follow the server from boot to a dashboard answering over HTTPS: the
/// provider's action, the installer's own steps, then the services and the
/// certificate, with DNS checked alongside throughout.
#[allow(clippy::too_many_arguments)]
async fn watch(
    plan: &Plan,
    settings: &Settings,
    cloud: &Cloud,
    server_id: &str,
    mut action: Option<String>,
    ip: Ipv4Addr,
    progress_token: &str,
    progress: &mut Progress,
) -> Result<(), Failure> {
    let kind = cloud.kind();
    let t = Instant::now();
    let reporter = Reporter::new(ip, progress_token, settings.progress_via.as_ref())?;
    let instance = Instance::new(&plan.domain, IpAddr::V4(ip), settings.instance_via.as_ref())?;
    let mut phase = Phase::Booting;
    let mut dns_ok = false;
    // The install timeout runs from when DNS is right (the certificate
    // can't come earlier); until then --dns-timeout applies.
    let mut install_clock: Option<Instant> = None;
    // Installer steps shown so far: (name, state).
    let mut shown: Vec<(String, String)> = Vec::new();
    let mut reporting = false;
    let mut detail = String::from("starting");
    progress.start("Booting Ubuntu");
    loop {
        if let Some(id) = action.clone() {
            match cloud
                .provider()
                .action(&id)
                .await
                .map_err(|e| Failure::from(cloud::error(kind, e)))?
            {
                ActionState::Done => {
                    action = None;
                    progress.event(&Event::ServerRunning {
                        id: json_id(server_id),
                        elapsed_secs: t.elapsed().as_secs(),
                    });
                    detail = "starting cloud-init".into();
                }
                ActionState::Failed(why) => {
                    return Err(anyhow::anyhow!(
                        "{} could not create the server: {why}",
                        kind.label()
                    )
                    .into());
                }
                ActionState::Running => {}
            }
        }

        if !dns_ok {
            match settings.dns.ipv4(&plan.domain).await {
                Ok(a) if dns::pointing(&a, ip) == Pointing::Here => {
                    dns_ok = true;
                    install_clock = Some(Instant::now());
                    progress.aside(&format!("{} points at {ip}", plan.domain));
                    progress.event(&Event::DnsReady {
                        name: plan.domain.clone(),
                        ip: ip.to_string(),
                        elapsed_secs: t.elapsed().as_secs(),
                    });
                }
                _ if t.elapsed() > settings.dns_timeout => {
                    return Err(fail(
                        anyhow::anyhow!(
                            "{} didn't point at {ip} within {}",
                            plan.domain,
                            duration(settings.dns_timeout)
                        ),
                        format!(
                            "Add an A record {} → {ip} (DNS only, not proxied) and run again, or pass --cloudflare-dns.",
                            plan.domain
                        ),
                    ));
                }
                _ => {}
            }
        }

        if matches!(phase, Phase::Booting | Phase::Installer) {
            match reporter.fetch().await {
                Ok(report) => {
                    reporting = true;
                    if phase == Phase::Booting {
                        progress.done("Booting Ubuntu");
                        phase = Phase::Installer;
                    }
                    show_steps(&report, &mut shown, progress);
                    match report.state.as_str() {
                        "failed" => {
                            let e = report.error.unwrap_or(InstallError {
                                step: String::new(),
                                message: "the installer failed".into(),
                                hint: String::new(),
                            });
                            progress.event(&Event::InstallFailed {
                                step: e.step.clone(),
                                message: e.message.clone(),
                                hint: e.hint.clone(),
                            });
                            let hint = [
                                e.hint.as_str(),
                                "The full output is in /var/log/cloud-init-output.log on the server.",
                            ]
                            .iter()
                            .filter(|h| !h.is_empty())
                            .copied()
                            .collect::<Vec<_>>()
                            .join(" ");
                            let step = if e.step.is_empty() {
                                String::new()
                            } else {
                                format!("{}: ", friendly(&e.step))
                            };
                            return Err(fail(
                                anyhow::anyhow!("installing failed at {step}{}", e.message),
                                hint,
                            ));
                        }
                        "handoff" => {
                            finish_steps(&mut shown, progress);
                            phase = Phase::Starting;
                            progress.start("Starting services");
                        }
                        _ => {}
                    }
                }
                // It was there and is gone: the installer handed port 80 over.
                Err(_) if reporting => {
                    finish_steps(&mut shown, progress);
                    phase = Phase::Starting;
                    progress.start("Starting services");
                }
                Err(_) => {}
            }
        }

        // The dashboard itself (also covers an installer without progress
        // reporting: it just answers one day).
        if phase != Phase::Installer && action.is_none() {
            match instance.health().await {
                Ok(()) if dns_ok => {
                    if phase == Phase::Booting {
                        progress.done("Booting Ubuntu");
                        progress.start("Installing Modula");
                    }
                    progress.done("Dashboard is up (HTTPS)");
                    return Ok(());
                }
                Ok(()) => detail = format!("waiting for {} → {ip}", plan.domain),
                Err(why) => {
                    if phase == Phase::Starting && looks_like_tls(&why) {
                        progress.done("Services started");
                        phase = Phase::Certificate;
                        progress.start("Getting a TLS certificate");
                    }
                    detail = if phase == Phase::Certificate && !dns_ok {
                        format!("waiting for {} → {ip}", plan.domain)
                    } else if phase == Phase::Certificate {
                        "Let's Encrypt".into()
                    } else {
                        why
                    };
                }
            }
        }

        if install_clock.is_some_and(|c| c.elapsed() > settings.install_timeout) {
            return Err(fail(
                anyhow::anyhow!(
                    "the dashboard didn't answer within {} ({detail})",
                    duration(settings.install_timeout)
                ),
                format!(
                    "The installer logs to /var/log/cloud-init-output.log on the server{}. \
                     Certificates need {} to point at {ip} and ports 80/443 open.",
                    match plan.ssh {
                        SshChoice::None => String::new(),
                        _ => format!(
                            " (ssh {}@{ip} tail -f /var/log/cloud-init-output.log)",
                            kind.ssh_user()
                        ),
                    },
                    plan.domain
                ),
            ));
        }
        let shown_detail = if phase == Phase::Installer {
            None
        } else {
            Some(detail.as_str())
        };
        pause(progress, settings.poll, shown_detail).await;
    }
}

/// Render what changed in the installer's step list since last time.
fn show_steps(report: &Report, shown: &mut Vec<(String, String)>, progress: &mut Progress) {
    for (i, step) in report.steps.iter().enumerate() {
        let before = shown.get(i).map(|(_, s)| s.as_str());
        if before == Some(step.state.as_str()) {
            continue;
        }
        let name = friendly(&step.name);
        match (before, step.state.as_str()) {
            (_, "skipped") => progress.skipped(&name),
            (None, "running") => progress.start(&name),
            (None, "done") => {
                progress.start(&name);
                progress.done(&name);
            }
            (Some("running"), "done") => progress.done(&name),
            _ => {}
        }
        match shown.get_mut(i) {
            Some(s) => s.1 = step.state.clone(),
            None => shown.push((step.name.clone(), step.state.clone())),
        }
    }
}

/// A step still shown as running when the installer went quiet finished.
fn finish_steps(shown: &mut [(String, String)], progress: &mut Progress) {
    if let Some(last) = shown.last_mut()
        && last.1 == "running"
    {
        progress.done(&friendly(&last.0));
        last.1 = "done".into();
    }
}

/// Whether a failed HTTPS check means "no certificate yet".
fn looks_like_tls(why: &str) -> bool {
    let l = why.to_ascii_lowercase();
    l.contains("certificate") || l.contains("tls") || l.contains("handshake") || l.contains("ssl")
}

/// Wait `total`, redrawing the spinner every quarter second.
async fn pause(progress: &mut Progress, total: Duration, detail: Option<&str>) {
    let step = Duration::from_millis(250).min(total);
    let until = Instant::now() + total;
    while Instant::now() < until {
        progress.tick(detail);
        tokio::time::sleep(step).await;
    }
}

/// Create the admin with the setup token, then an API token for this
/// machine. Returns (instance URL to save, API token).
pub async fn finish_setup(
    plan: &Plan,
    settings: &Settings,
    ip: Ipv4Addr,
    setup_token: &SecretString,
    (admin_name, admin_password): (&str, &SecretString),
    secrets: &Secrets,
    progress: &mut Progress,
) -> Result<(Url, SecretString), Failure> {
    let instance = Instance::new(&plan.domain, IpAddr::V4(ip), settings.instance_via.as_ref())?;
    progress.start("Creating your admin account");
    if !instance.setup_required().await? {
        return Err(fail(
            anyhow::anyhow!("setup was already completed on {}", plan.domain),
            format!("Log in with `modula login {}`.", plan.domain),
        ));
    }
    let session = instance
        .setup(setup_token, &plan.email, admin_name, admin_password)
        .await?;
    progress.done(&format!("Admin account created ({})", plan.email));
    progress.event(&Event::SetupComplete {
        email: plan.email.clone(),
    });
    progress.start("Creating an API token for this machine");
    let host = hostname();
    let token = instance
        .create_token(&session, &format!("modula CLI on {host}"))
        .await?;
    progress.done("API token created");

    // The tokens this run used become the workspace's integrations. Best
    // effort: the server works without them (Settings → Integrations).
    progress.start(&format!(
        "Saving your {} and Cloudflare tokens to the server",
        secrets.cloud.provider().label()
    ));
    let account = match &plan.dns {
        DnsPlan::Cloudflare { zone, .. } => zone.account.as_ref().map(|a| a.id.as_str()),
        _ => None,
    };
    let cloudflare = secrets.cloudflare.as_ref().map(|t| (t, account));
    match instance
        .save_integrations(&session, &secrets.cloud, cloudflare)
        .await
    {
        Ok(saved) => {
            let ok: Vec<&str> = saved
                .iter()
                .filter(|(_, r)| r.is_ok())
                .map(|(n, _)| *n)
                .collect();
            if ok.is_empty() {
                progress.done("Tokens not saved");
            } else {
                progress.done(&format!("Connected on the server: {}", ok.join(" and ")));
            }
            for (name, r) in &saved {
                if let Err(e) = r {
                    progress.note(&format!(
                        "{name} wasn't saved ({e:#}); connect it in Settings → Integrations"
                    ));
                }
            }
        }
        Err(e) => {
            progress.done("Tokens not saved");
            progress.note(&format!(
                "couldn't save the tokens ({e:#}); connect them in Settings → Integrations"
            ));
        }
    }
    Ok((instance.base().clone(), token))
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "this machine".into())
}

/// What cleaning up removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cleaned {
    pub server: bool,
    pub firewall: bool,
    pub dns_record: bool,
}

/// Delete what this run created: the server (on EC2 with its Elastic IP),
/// then its firewall (Hetzner refuses while the server still holds it, so
/// retry for a minute), then the DNS record. An uploaded SSH key stays: it's
/// yours, and harmless. So do EC2 security groups: free, and reused by the
/// next attempt with this name.
pub async fn cleanup(
    created: &Created,
    secrets: &Secrets,
    settings: &Settings,
) -> anyhow::Result<Cleaned> {
    let cloud = Cloud::connect(&secrets.cloud, settings, "cli")?;
    let kind = cloud.kind();
    let mut done = Cleaned::default();
    if let Some(id) = &created.server {
        cloud
            .provider()
            .delete(id)
            .await
            .map_err(|e| cloud::error(kind, e))?;
        done.server = true;
    }
    if let Some(id) = &created.firewall {
        let t = Instant::now();
        loop {
            match cloud.delete_firewall(id).await {
                Ok(()) => {
                    done.firewall = true;
                    break;
                }
                Err(ProviderError::Api { code, .. })
                    if code == "resource_in_use" && t.elapsed() < Duration::from_secs(90) =>
                {
                    tokio::time::sleep(settings.poll.min(Duration::from_secs(3))).await;
                }
                Err(e) => bail!("deleting the firewall failed: {}", cloud::error(kind, e)),
            }
        }
    }
    if let (Some((zone, record)), Some(token)) = (&created.dns_record, &secrets.cloudflare) {
        let cf = cloudflare::Client::new(settings.cloudflare_api.clone(), token.clone())?;
        cf.delete_record(zone, record).await?;
        done.dns_record = true;
    }
    Ok(done)
}

/// A failed or interrupted create may still have made the server (or, on
/// EC2, taken an Elastic IP): find it by its label so it can be deleted.
/// `find` also releases an Elastic IP left without a server. Best effort.
pub async fn recover(created: &mut Created, secrets: &Secrets, settings: &Settings) {
    let (None, Some(modula_id)) = (&created.server, &created.modula_id) else {
        return;
    };
    let Ok(cloud) = Cloud::connect(&secrets.cloud, settings, "cli") else {
        return;
    };
    if let Ok(Some(server)) = cloud.provider().find(modula_id).await {
        created.server = Some(server.id);
    }
}

/// A fresh setup token (re-exported for the command).
pub fn new_setup_token() -> anyhow::Result<SecretString> {
    setup_token::generate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_failures_mean_no_certificate_yet() {
        assert!(looks_like_tls("invalid peer certificate: UnknownIssuer"));
        assert!(looks_like_tls("received fatal alert: InternalError (TLS)"));
        assert!(!looks_like_tls("connection refused"));
        assert!(!looks_like_tls("HTTP 502"));
    }

    #[test]
    fn installer_steps_render_once_each() {
        let (out, buf) = crate::output::Output::capture(true);
        let mut p = Progress::new(out);
        let mut shown = Vec::new();
        let report = |steps: &[(&str, &str)]| Report {
            state: "running".into(),
            steps: steps
                .iter()
                .map(|(n, s)| super::super::install_progress::Step {
                    name: (*n).into(),
                    state: (*s).into(),
                })
                .collect(),
            error: None,
        };
        show_steps(
            &report(&[("Checking this server", "running")]),
            &mut shown,
            &mut p,
        );
        show_steps(
            &report(&[("Checking this server", "running")]),
            &mut shown,
            &mut p,
        );
        show_steps(
            &report(&[
                ("Checking this server", "done"),
                ("Firewall", "skipped"),
                ("Installing packages", "running"),
            ]),
            &mut shown,
            &mut p,
        );
        finish_steps(&mut shown, &mut p);
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let events: Vec<(String, String)> = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .map(|v| {
                (
                    v["name"].as_str().unwrap().to_owned(),
                    v["state"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let pkgs = "Updating packages, installing Docker, Postgres & Redis";
        assert_eq!(
            events,
            [
                ("Checking the server".into(), "running".into()),
                ("Checking the server".into(), "done".into()),
                ("Firewall".into(), "skipped".into()),
                (pkgs.into(), "running".into()),
                (pkgs.into(), "done".into()),
            ]
        );
    }
}
