//! `modula server create` and `modula server options`: a Modula server on
//! Hetzner Cloud, created from a laptop; `modula server resize` ([`resize`])
//! changes its size later. (`server update` lives in
//! [`crate::commands::update`].)
//!
//! The answers come from a wizard ([`wizard`]) or, for scripts and agents,
//! from flags and environment variables (checked all at once up front, so
//! nothing is created when one is missing). [`flow`] then creates the
//! firewall and server, waits for DNS and the install, finishes setup with a
//! setup token generated here ([`setup_token`]) and logs this CLI in.
//!
//! Secrets (the Hetzner and Cloudflare tokens, the admin password) are
//! [`SecretString`]s: zeroized when dropped, never in argv, logs, files or
//! the server's user data. Once the admin exists, the tokens are saved on
//! the new server as the workspace's integrations (encrypted there).

pub mod catalog;
pub mod cloud_init;
pub mod cloudflare;
pub mod dns;
pub mod flow;
pub mod install_progress;
pub mod instance;
pub mod progress;
pub mod resize;
pub mod setup_token;
pub mod wizard;

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use modula_hetzner::{Client, SshKey};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;

use self::catalog::{LocationOption, format_eur};
use self::flow::{Created, DnsPlan, Failure, Plan, Secrets, Settings, SshChoice};
use self::progress::{Event, Progress, duration};
use crate::api::Api;
use crate::cli::{OnFailure, ServerCommand, ServerCreateArgs, ServerOptionsArgs};
use crate::config::{self, SavedLogin};
use crate::context::Ctx;
use crate::output::{Table, usage};

pub const ENV_HCLOUD: &str = "HCLOUD_TOKEN";
pub const ENV_CLOUDFLARE: &str = "CLOUDFLARE_API_TOKEN";
pub const ENV_ADMIN_PASSWORD: &str = "MODULA_ADMIN_PASSWORD";
pub const DEFAULT_NAME: &str = "modula-main";
pub const DEFAULT_ADMIN_NAME: &str = "Admin";

/// Exit code: a server exists (and bills) but isn't fully set up.
pub const EXIT_SERVER_KEPT: i32 = 3;

/// Secrets from the environment (or stdin), never from flags.
#[derive(Default)]
pub struct EnvSecrets {
    pub hcloud: Option<SecretString>,
    pub cloudflare: Option<SecretString>,
    pub admin_password: Option<SecretString>,
}

impl EnvSecrets {
    pub fn from_process() -> Self {
        let var = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .map(SecretString::from)
        };
        Self {
            hcloud: var(ENV_HCLOUD),
            cloudflare: var(ENV_CLOUDFLARE),
            admin_password: std::env::var(ENV_ADMIN_PASSWORD)
                .ok()
                .filter(|v| !v.is_empty())
                .map(SecretString::from),
        }
    }

    /// `--token-stdin`: the first line of stdin is the Hetzner token.
    fn read_hcloud_from_stdin(&mut self) -> anyhow::Result<()> {
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("cannot read the token from stdin")?;
        let token = line.trim().to_owned();
        zeroize::Zeroize::zeroize(&mut line);
        if token.is_empty() {
            return Err(usage("--token-stdin: stdin had no token on its first line"));
        }
        self.hcloud = Some(SecretString::from(token));
        Ok(())
    }
}

pub async fn run(ctx: &mut Ctx, command: ServerCommand) -> anyhow::Result<i32> {
    match command {
        ServerCommand::Create(args) => {
            reject_secret_flags(&args)?;
            let mut env = EnvSecrets::from_process();
            if args.token_stdin {
                env.read_hcloud_from_stdin()?;
            }
            let settings = Settings::production(
                Duration::from_secs(args.dns_timeout.max(1) * 60),
                Duration::from_secs(args.install_timeout.max(1) * 60),
            );
            create_with(ctx, *args, env, settings).await
        }
        ServerCommand::Options(args) => {
            let mut env = EnvSecrets::from_process();
            if args.token_stdin {
                env.read_hcloud_from_stdin()?;
            }
            let settings = Settings::production(Duration::ZERO, Duration::ZERO);
            options_with(ctx, &args, &env, &settings).await?;
            Ok(0)
        }
        ServerCommand::Update { check } => crate::commands::update::server(ctx, check).await,
        ServerCommand::Resize(args) => {
            let mut env = EnvSecrets::from_process();
            if args.token_stdin {
                env.read_hcloud_from_stdin()?;
            }
            let settings = Settings::production(Duration::ZERO, Duration::ZERO);
            resize::run(ctx, &args, env, &settings).await
        }
    }
}

/// Secrets passed as flags are refused: they'd sit in shell history and in
/// every process listing. (Already exposed, sadly; say so.)
fn reject_secret_flags(args: &ServerCreateArgs) -> anyhow::Result<()> {
    let (flag, env) = if args.hcloud_token.is_some() {
        ("--hcloud-token", "HCLOUD_TOKEN (or --token-stdin)")
    } else if args.cloudflare_token.is_some() {
        ("--cloudflare-token", ENV_CLOUDFLARE)
    } else if args.admin_password.is_some() {
        ("--admin-password", ENV_ADMIN_PASSWORD)
    } else {
        return Ok(());
    };
    Err(usage(format!(
        "{flag} isn't accepted: secrets in arguments end up in shell history and process lists. \
         Set {env} instead (and consider rotating the value you just typed)."
    )))
}

/// Whether to prompt: a terminal on stdin, and not `--json` (machine
/// output never waits for a person).
pub fn interactive(ctx: &Ctx) -> bool {
    ctx.session.interactive && !ctx.global.json
}

/// Without prompts: every flag and variable that's still needed, so one run
/// says everything that's missing.
pub fn missing_inputs(args: &ServerCreateArgs, env: &EnvSecrets, yes: bool) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if env.hcloud.is_none() {
        missing.push("HCLOUD_TOKEN (environment) or --token-stdin");
    }
    if args.location.is_none() {
        missing.push("--location");
    }
    if args.server_type.is_none() {
        missing.push("--type");
    }
    if args.domain.is_none() {
        missing.push("--domain");
    }
    if args.email.is_none() {
        missing.push("--email");
    }
    if args.ssh_key_file.is_none() && args.ssh_key_id.is_none() && !args.no_ssh_key {
        missing.push("--ssh-key-file, --ssh-key-id or --no-ssh-key");
    }
    if args.cloudflare_dns && env.cloudflare.is_none() {
        missing.push("CLOUDFLARE_API_TOKEN (environment, for --cloudflare-dns)");
    }
    if env.admin_password.is_none() {
        missing.push("MODULA_ADMIN_PASSWORD (environment)");
    }
    if !yes {
        missing.push("--yes (creating a server costs money)");
    }
    missing
}

/// Formats that can be checked without the network.
pub fn check_values(args: &ServerCreateArgs, env: &EnvSecrets) -> anyhow::Result<()> {
    if let Some(d) = args.domain.as_deref().filter(|d| !is_auto(d)) {
        valid_domain(d).map_err(|e| usage(format!("--domain: {e}")))?;
    }
    if let Some(e) = &args.email {
        valid_email(e).map_err(|e| usage(format!("--email: {e}")))?;
    }
    if let Some(n) = &args.name {
        valid_server_name(n).map_err(|e| usage(format!("--name: {e}")))?;
    }
    if let Some(n) = &args.admin_name {
        valid_admin_name(n).map_err(|e| usage(format!("--admin-name: {e}")))?;
    }
    if let Some(p) = &env.admin_password {
        valid_password(p.expose_secret())
            .map_err(|e| usage(format!("{ENV_ADMIN_PASSWORD}: {e}")))?;
    }
    if let Some(u) = &args.install_url {
        let ok = url::Url::parse(u).is_ok_and(|u| u.scheme() == "https");
        if !ok {
            return Err(usage("--install-url must be an https:// URL"));
        }
    }
    Ok(())
}

/// `--domain auto`: no domain yet, the dashboard at `https://<ip>`.
pub fn is_auto(domain: &str) -> bool {
    domain.trim().eq_ignore_ascii_case("auto")
}

/// A hostname like deploy.example.com (what install.sh accepts).
pub fn valid_domain(raw: &str) -> Result<String, String> {
    let d = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = d.split('.').collect();
    let label_ok = |l: &&str| {
        let b = l.as_bytes();
        (1..=63).contains(&b.len())
            && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
            && b[0] != b'-'
            && b[b.len() - 1] != b'-'
    };
    let tld_ok = labels
        .last()
        .is_some_and(|t| (2..=63).contains(&t.len()) && t.bytes().all(|c| c.is_ascii_lowercase()));
    if d.len() <= 253 && labels.len() >= 2 && labels.iter().all(label_ok) && tld_ok {
        Ok(d)
    } else {
        Err("must be a hostname like deploy.example.com (no https://, no path)".into())
    }
}

/// The server's own rules (modula-server's `validate::email`).
pub fn valid_email(raw: &str) -> Result<String, String> {
    let e = raw.trim().to_lowercase();
    let valid = e.len() <= 254
        && !e
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\'')
        && matches!(e.split_once('@'), Some((local, domain))
            if !local.is_empty() && domain.contains('.') && !domain.contains('@')
               && !domain.starts_with('.') && !domain.ends_with('.'));
    if valid {
        Ok(e)
    } else {
        Err("must be an email address like you@example.com".into())
    }
}

/// Server names become hostnames: lowercase DNS labels.
pub fn valid_server_name(raw: &str) -> Result<String, String> {
    let b = raw.as_bytes();
    let ok = (1..=63).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-';
    if ok {
        Ok(raw.to_owned())
    } else {
        Err("lowercase letters, digits and - (not at either end), up to 63".into())
    }
}

pub fn valid_admin_name(raw: &str) -> Result<String, String> {
    let n = raw.trim();
    let len = n.chars().count();
    if len == 0 || len > 64 || n.chars().any(char::is_control) {
        return Err("must be 1–64 characters".into());
    }
    Ok(n.to_owned())
}

/// modula-server's password policy.
pub fn valid_password(p: &str) -> Result<(), String> {
    if p.chars().count() < 10 {
        return Err("the password must be at least 10 characters".into());
    }
    if p.len() > 256 {
        return Err("the password must be at most 256 bytes".into());
    }
    Ok(())
}

/// Locations and types, boiled down ([`catalog::build`]).
pub async fn load_catalog(hz: &Client) -> anyhow::Result<Vec<LocationOption>> {
    let (locations, types) =
        futures::try_join!(hz.locations(), hz.server_types()).map_err(flow::hetzner_error)?;
    Ok(catalog::build(&locations, &types))
}

/// `ssh-ed25519 AAAA…` without the comment, for comparing keys.
pub fn key_body(public_key: &str) -> String {
    public_key
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A public key file's single key line; refuses private keys.
pub fn read_public_key(path: &Path) -> anyhow::Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    if text.contains("PRIVATE KEY") {
        anyhow::bail!(
            "{} is a private key; pass the public one ({}.pub)",
            path.display(),
            path.display()
        );
    }
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .with_context(|| format!("{} is empty", path.display()))?;
    let kind = line.split_whitespace().next().unwrap_or("");
    let known = kind.starts_with("ssh-") || kind.starts_with("ecdsa-") || kind.starts_with("sk-");
    if !known || line.split_whitespace().count() < 2 {
        anyhow::bail!("{} doesn't look like an OpenSSH public key", path.display());
    }
    Ok(line.to_owned())
}

/// `~/.ssh/*.pub`, sorted.
pub fn local_public_keys() -> Vec<(PathBuf, String)> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dir = PathBuf::from(home).join(".ssh");
    let mut keys: Vec<(PathBuf, String)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "pub"))
        .filter_map(|p| read_public_key(&p).ok().map(|k| (p, k)))
        .collect();
    keys.sort();
    keys
}

/// Reuse the Hetzner key with the same public key, else upload it under a
/// name not taken yet.
pub fn ssh_choice_for_file(path: &Path, public_key: String, existing: &[SshKey]) -> SshChoice {
    let body = key_body(&public_key);
    if let Some(k) = existing.iter().find(|k| key_body(&k.public_key) == body) {
        return SshChoice::Existing {
            id: k.id,
            name: k.name.clone(),
        };
    }
    let comment = public_key.split_whitespace().nth(2).map(str::to_owned);
    let base = comment.unwrap_or_else(|| {
        path.file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "modula".into())
    });
    let mut name = base.clone();
    let mut n = 2;
    while existing.iter().any(|k| k.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    SshChoice::Upload {
        name,
        public_key,
        path: path.to_owned(),
    }
}

/// The zone for `domain` and the records an A record would compete with.
pub async fn cloudflare_lookup(
    cf: &cloudflare::Client,
    domain: &str,
) -> anyhow::Result<(cloudflare::Zone, Vec<cloudflare::DnsRecord>)> {
    cf.verify().await?;
    let zone = cf.find_zone(domain).await?.with_context(|| {
        format!("the Cloudflare token can't see a zone for {domain} (is the domain on this Cloudflare account?)")
    })?;
    let records = cf.address_records(&zone.id, domain).await?;
    Ok((zone, records))
}

pub fn describe_records(records: &[cloudflare::DnsRecord]) -> String {
    records
        .iter()
        .map(|r| format!("{} {}", r.kind, r.content))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Everything from flags and the environment, checked against Hetzner (and
/// Cloudflare) before anything is created. Mismatches are usage errors.
async fn plan_from_flags(
    args: &ServerCreateArgs,
    env: &EnvSecrets,
    settings: &Settings,
) -> anyhow::Result<Plan> {
    let token = env.hcloud.clone().context("no Hetzner token")?;
    let hz = flow::hetzner(settings, &token)?;
    let catalog = load_catalog(&hz).await?;
    let wanted = args.location.as_deref().unwrap_or_default();
    let location = catalog
        .iter()
        .find(|l| l.name.eq_ignore_ascii_case(wanted))
        .cloned()
        .ok_or_else(|| {
            usage(format!(
                "--location {wanted}: no suitable server type can be created there now. Available: {}",
                catalog.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ")
            ))
        })?;
    let wanted = args.server_type.as_deref().unwrap_or_default();
    let server_type = location.find_type(wanted).cloned().ok_or_else(|| {
        usage(format!(
            "--type {wanted} isn't available in {} now. Available there: {} (recommended: {})",
            location.name,
            location
                .types
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            location.recommended.as_deref().unwrap_or("-")
        ))
    })?;
    let ssh = if let Some(path) = &args.ssh_key_file {
        let key = read_public_key(path).map_err(|e| usage(format!("--ssh-key-file: {e:#}")))?;
        let existing = hz.ssh_keys().await.map_err(flow::hetzner_error)?;
        ssh_choice_for_file(path, key, &existing)
    } else if let Some(id) = args.ssh_key_id {
        let existing = hz.ssh_keys().await.map_err(flow::hetzner_error)?;
        let k = existing.iter().find(|k| k.id == id).ok_or_else(|| {
            usage(format!(
                "--ssh-key-id {id}: no such key in this Hetzner project. Keys: {}",
                existing
                    .iter()
                    .map(|k| format!("{} ({})", k.id, k.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        SshChoice::Existing {
            id: k.id,
            name: k.name.clone(),
        }
    } else {
        SshChoice::None
    };
    let auto = args.domain.as_deref().is_some_and(is_auto);
    if auto && args.cloudflare_dns {
        return Err(usage("--domain auto needs no DNS; drop --cloudflare-dns"));
    }
    let domain = if auto {
        String::new()
    } else {
        valid_domain(args.domain.as_deref().unwrap_or_default()).map_err(usage)?
    };
    let dns = if auto {
        DnsPlan::Instant
    } else if args.cloudflare_dns {
        let token = env.cloudflare.clone().context("no Cloudflare token")?;
        let cf = cloudflare::Client::new(settings.cloudflare_api.clone(), token)?;
        let (zone, records) = cloudflare_lookup(&cf, &domain).await?;
        if !records.is_empty() && !args.replace_dns {
            return Err(usage(format!(
                "{domain} already has DNS records at Cloudflare ({}); pass --replace-dns to replace them",
                describe_records(&records)
            )));
        }
        DnsPlan::Cloudflare {
            zone,
            replace: records,
        }
    } else {
        DnsPlan::Manual
    };
    Ok(Plan {
        location,
        server_type,
        name: args.name.clone().unwrap_or_else(|| DEFAULT_NAME.into()),
        domain,
        email: valid_email(args.email.as_deref().unwrap_or_default()).map_err(usage)?,
        ssh,
        dns,
        install_url: args
            .install_url
            .clone()
            .unwrap_or_else(|| cloud_init::INSTALL_URL.into()),
    })
}

/// `server create` with the given secrets and endpoints (tests pass mocks).
pub async fn create_with(
    ctx: &mut Ctx,
    args: ServerCreateArgs,
    mut env: EnvSecrets,
    settings: Settings,
) -> anyhow::Result<i32> {
    let prompt = interactive(ctx);
    check_values(&args, &env)?;
    if !prompt {
        let missing = missing_inputs(&args, &env, ctx.global.yes);
        if !missing.is_empty() {
            return Err(usage(format!(
                "without a terminal to ask on (or with --json), these are required:\n  {}\nRun `modula server create --help` for an example.",
                missing.join("\n  ")
            )));
        }
    }

    let (mut plan, secrets) = if prompt {
        match wizard::run(ctx, &args, &mut env, &settings).await? {
            Some(answers) => answers,
            None => {
                ctx.out.note("  Okay, nothing was created.");
                return Ok(1);
            }
        }
    } else {
        let plan = plan_from_flags(&args, &env, &settings).await?;
        let secrets = Secrets {
            hetzner: env.hcloud.clone().context("no Hetzner token")?,
            cloudflare: env.cloudflare.clone(),
        };
        (plan, secrets)
    };

    let mut progress = Progress::new(ctx.out.clone());
    progress.event(&Event::Plan {
        provider: "hetzner".into(),
        location: plan.location.name.clone(),
        server_type: plan.server_type.name.clone(),
        name: plan.name.clone(),
        domain: if matches!(plan.dns, DnsPlan::Instant) {
            "<ip>".into()
        } else {
            plan.domain.clone()
        },
        monthly_cents: plan.server_type.monthly_cents,
        monthly: plan.server_type.monthly.clone(),
    });
    if !progress.json() {
        ctx.out.note("");
    }
    let setup_token = flow::new_setup_token()?;
    let mut created = Created::default();

    let provisioned = tokio::select! {
        r = flow::provision(&mut plan, &secrets, &settings, &setup_token, &mut progress, &mut created) => r,
        _ = tokio::signal::ctrl_c() => Err(Failure {
            error: anyhow::anyhow!("interrupted"),
            hint: None,
        }),
    };
    let ip = match provisioned {
        Ok(ip) => ip,
        Err(f) => {
            return fail_and_clean(
                ctx,
                &args,
                &mut progress,
                &plan,
                &secrets,
                &settings,
                &created,
                f,
            )
            .await;
        }
    };

    // The server answers: create the admin and log in. From here on the
    // server works, so a failure keeps it and hands over the setup token.
    let admin = if prompt && (args.admin_name.is_none() || env.admin_password.is_none()) {
        wizard::ask_admin(
            &plan.email,
            args.admin_name.as_deref(),
            env.admin_password.take(),
        )
    } else {
        env.admin_password
            .take()
            .context("no admin password")
            .map(|p| {
                let name = args
                    .admin_name
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ADMIN_NAME.into());
                (name, p)
            })
    };
    let finished = match admin {
        Ok((admin_name, admin_password)) => {
            flow::finish_setup(
                &plan,
                &settings,
                ip,
                &setup_token,
                (&admin_name, &admin_password),
                &secrets,
                &mut progress,
            )
            .await
        }
        Err(e) => Err(e.into()),
    };
    let (base, api_token) = match finished {
        Ok(v) => v,
        Err(f) => {
            progress.failed();
            let url = format!("https://{}", plan.domain);
            let hint = format!(
                "The server is running; finish setup at {url}/setup with setup token {} (single use).",
                setup_token.expose_secret()
            );
            progress.event(&Event::Error {
                message: format!("{:#}", f.error),
                hint: Some(hint.clone()),
            });
            if !progress.json() {
                ctx.out.note(format!("\n  ✗ {:#}", f.error));
            }
            return finish(
                ctx,
                &progress,
                &plan,
                &created,
                Err((f.error, Some(hint), EXIT_SERVER_KEPT)),
            );
        }
    };

    // Saved like `modula login` does (named after the domain, now the
    // default); a new instance has no Access settings.
    let url = format!("https://{}/", plan.domain);
    let name = config::default_login_name(&config::parse_url(&url)?);
    ctx.cfg.set(
        &name,
        SavedLogin {
            url: url.clone(),
            token: api_token.expose_secret().to_owned(),
            access: None,
        },
    );
    config::save(&ctx.session.config_path, &ctx.cfg)?;
    let check = SavedLogin {
        url: base.to_string(),
        token: api_token.expose_secret().to_owned(),
        access: None,
    };
    let me = Api::new(config::login(Some(&check), None, None)?)?
        .me()
        .await?;
    progress.event(&Event::LoggedIn {
        url: url.clone(),
        email: me.user.email.clone(),
    });
    progress.event(&Event::Ready {
        url: url.trim_end_matches('/').to_owned(),
        ip: ip.to_string(),
        server_id: created.server.unwrap_or_default(),
        elapsed_secs: progress.elapsed().as_secs(),
    });
    stale_dns_hint(&mut progress, &plan.domain).await;
    finish(ctx, &progress, &plan, &created, Ok(me.user.email))
}

/// The new domain may be cached here as "no such name" (looked up before
/// its record existed). The CLI asks public DNS when that happens, but a
/// browser doesn't: say how to clear the cache.
async fn stale_dns_hint(progress: &mut Progress, domain: &str) {
    if domain.is_empty() || domain.parse::<std::net::IpAddr>().is_ok() {
        return;
    }
    let found = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::lookup_host((domain, 443)),
    )
    .await;
    if let Ok(Ok(mut addrs)) = found
        && addrs.next().is_some()
    {
        return;
    }
    let flush = if cfg!(target_os = "macos") {
        "sudo dscacheutil -flushcache; sudo killall -HUP mDNSResponder"
    } else if cfg!(windows) {
        "ipconfig /flushdns"
    } else {
        "resolvectl flush-caches"
    };
    progress.note(&format!(
        "This computer doesn't see {domain} yet (an old \"no such name\" answer is cached, \
         for up to 30 minutes). modula works already; if your browser can't open it, \
         clear the cache: {flush}"
    ));
}

/// After a failure with a server around: delete it (and what came with it)
/// per --on-failure, then report.
#[allow(clippy::too_many_arguments)]
async fn fail_and_clean(
    ctx: &Ctx,
    args: &ServerCreateArgs,
    progress: &mut Progress,
    plan: &Plan,
    secrets: &Secrets,
    settings: &Settings,
    created: &Created,
    f: Failure,
) -> anyhow::Result<i32> {
    progress.failed();
    let message = format!("{:#}", f.error);
    progress.event(&Event::Error {
        message: message.clone(),
        hint: f.hint.clone(),
    });
    if !progress.json() {
        ctx.out.note(format!("\n  ✗ {message}"));
        if let Some(h) = &f.hint {
            ctx.out.note(format!("    {h}"));
        }
    }
    let anything =
        created.server.is_some() || created.firewall.is_some() || created.dns_record.is_some();
    if !anything {
        return finish(ctx, progress, plan, created, Err((f.error, f.hint, 1)));
    }
    let policy = args.on_failure.unwrap_or(if interactive(ctx) {
        OnFailure::Ask
    } else {
        OnFailure::Delete
    });
    let delete = match policy {
        OnFailure::Delete => true,
        OnFailure::Keep => false,
        OnFailure::Ask if interactive(ctx) => wizard::ask_delete(plan, created)?,
        OnFailure::Ask => false,
    };
    if !delete {
        let hint = match created.ip {
            Some(ip) => format!(
                "Kept the server ({ip}); it bills until you delete it in the Hetzner console. {}",
                f.hint.unwrap_or_default()
            ),
            None => "Kept what was created; delete it in the Hetzner console.".into(),
        };
        let code = if created.server.is_some() {
            EXIT_SERVER_KEPT
        } else {
            1
        };
        return finish(
            ctx,
            progress,
            plan,
            created,
            Err((f.error, Some(hint), code)),
        );
    }
    progress.start("Deleting the server and its firewall");
    match flow::cleanup(created, secrets, settings).await {
        Ok(c) => {
            progress.done("Deleted; nothing keeps billing");
            progress.event(&Event::Cleanup {
                server_deleted: c.server,
                firewall_deleted: c.firewall,
                dns_record_deleted: c.dns_record,
            });
            let mut gone = created.clone();
            gone.server = None;
            finish(ctx, progress, plan, &gone, Err((f.error, f.hint, 1)))
        }
        Err(e) => {
            progress.failed();
            let hint = format!(
                "Cleaning up failed too ({e:#}); delete server {} in the Hetzner console.",
                plan.name
            );
            finish(
                ctx,
                progress,
                plan,
                created,
                Err((f.error, Some(hint), EXIT_SERVER_KEPT)),
            )
        }
    }
}

/// The closing card (people) or `result` event (`--json`); the exit code.
fn finish(
    ctx: &Ctx,
    progress: &Progress,
    plan: &Plan,
    created: &Created,
    outcome: Result<String, (anyhow::Error, Option<String>, i32)>,
) -> anyhow::Result<i32> {
    let url = format!("https://{}", plan.domain);
    let server = created.server.map(|id| {
        json!({
            "id": id,
            "name": plan.name,
            "ip": created.ip.map(|ip| ip.to_string()),
            "location": plan.location.name,
            "type": plan.server_type.name,
            "monthlyCents": plan.server_type.monthly_cents,
        })
    });
    match outcome {
        Ok(email) => {
            if ctx.out.json {
                ctx.out.line(serde_json::to_string(&json!({
                    "event": "result", "ok": true, "exitCode": 0, "url": url,
                    "server": server, "loggedIn": true, "email": email,
                    "config": ctx.session.config_path,
                }))?);
                return Ok(0);
            }
            let o = &ctx.out;
            let bar = o.accent("│");
            let ip = created.ip.map(|i| i.to_string()).unwrap_or_default();
            o.line("");
            o.line(format!(
                "  {} Your Modula server is ready!  {}",
                o.accent("╭─"),
                o.dim(&format!("({})", duration(progress.elapsed())))
            ));
            o.line(format!("  {bar}"));
            o.line(format!("  {bar}  {:<14}{}", "Dashboard", o.accent(&url)));
            o.line(format!(
                "  {bar}  {:<14}{} · {} · {} · {ip}",
                "Server", plan.name, plan.server_type.name, plan.location.city
            ));
            o.line(format!(
                "  {bar}  {:<14}{} {}",
                "Cost",
                plan.server_type.monthly,
                o.dim("per month, billed by Hetzner")
            ));
            o.line(format!(
                "  {bar}  {:<14}{email} (this CLI is logged in)",
                "Admin"
            ));
            if !matches!(plan.ssh, SshChoice::None) {
                o.line(format!("  {bar}  {:<14}ssh root@{ip}", "SSH"));
            }
            o.line(format!("  {bar}"));
            o.line(format!(
                "  {bar}  {} {}",
                o.dim("Next:"),
                o.accent("modula init <name> && modula up")
            ));
            o.line(format!(
                "  {}  {}",
                o.accent("╰─"),
                o.dim("Have fun shipping things!")
            ));
            o.line("");
            Ok(0)
        }
        Err((error, hint, code)) => {
            if ctx.out.json {
                ctx.out.line(serde_json::to_string(&json!({
                    "event": "result", "ok": false, "exitCode": code,
                    "error": format!("{error:#}"), "hint": hint,
                    "serverKept": created.server.is_some(), "server": server,
                }))?);
            } else if let Some(h) = &hint
                && code == EXIT_SERVER_KEPT
            {
                ctx.out.note(format!("\n  {h}\n"));
            }
            Ok(code)
        }
    }
}

/// `server options`: what can be created where, for people and agents.
pub async fn options_with(
    ctx: &Ctx,
    args: &ServerOptionsArgs,
    env: &EnvSecrets,
    settings: &Settings,
) -> anyhow::Result<()> {
    let token = env.hcloud.clone().ok_or_else(|| {
        usage("set HCLOUD_TOKEN (a Hetzner Cloud API token) or pass --token-stdin")
    })?;
    let hz = flow::hetzner(settings, &token)?;
    let mut catalog = load_catalog(&hz).await?;
    if let Some(wanted) = &args.location {
        catalog.retain(|l| l.name.eq_ignore_ascii_case(wanted));
        if catalog.is_empty() {
            return Err(usage(format!(
                "--location {wanted}: nothing suitable can be created there now"
            )));
        }
    }
    let value = json!({
        "provider": "hetzner",
        "recommendation": format!(
            "4 vCPU / 8 GB (cx33 or cax21 class): {}",
            catalog::RECOMMENDATION
        ),
        "locations": catalog,
    });
    ctx.out.either(&value, |o| {
        for l in &catalog {
            o.line(format!(
                "{}  {}",
                l.label(),
                o.dim(&format!(
                    "from {}/mo",
                    l.from_cents().map(format_eur).unwrap_or_default()
                ))
            ));
            let mut t = Table::new(&["type", "vcpu", "ram", "disk", "arch", "€/month", ""]);
            for ty in &l.types {
                t.row([
                    ty.name.clone(),
                    ty.cores.to_string(),
                    format!("{} GB", ty.memory_gb),
                    format!("{} GB", ty.disk_gb),
                    ty.arch.clone(),
                    ty.monthly.clone(),
                    if ty.recommended {
                        "★ recommended".into()
                    } else {
                        String::new()
                    },
                ]);
            }
            o.table(&t);
            o.line("");
        }
        o.note(format!(
            "Prices include VAT. ★ = enough for {} (4 vCPU / 8 GB class).",
            catalog::RECOMMENDATION
        ));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_args() -> ServerCreateArgs {
        ServerCreateArgs {
            location: Some("fsn1".into()),
            server_type: Some("cx33".into()),
            domain: Some("deploy.example.com".into()),
            email: Some("you@example.com".into()),
            no_ssh_key: true,
            ..Default::default()
        }
    }

    fn full_env() -> EnvSecrets {
        EnvSecrets {
            hcloud: Some("h".to_owned().into()),
            cloudflare: None,
            admin_password: Some("correct horse battery".to_owned().into()),
        }
    }

    #[test]
    fn every_missing_input_is_listed_at_once() {
        let missing = missing_inputs(&ServerCreateArgs::default(), &EnvSecrets::default(), false);
        assert_eq!(
            missing,
            [
                "HCLOUD_TOKEN (environment) or --token-stdin",
                "--location",
                "--type",
                "--domain",
                "--email",
                "--ssh-key-file, --ssh-key-id or --no-ssh-key",
                "MODULA_ADMIN_PASSWORD (environment)",
                "--yes (creating a server costs money)",
            ]
        );
        assert!(missing_inputs(&full_args(), &full_env(), true).is_empty());
        let mut args = full_args();
        args.cloudflare_dns = true;
        assert_eq!(
            missing_inputs(&args, &full_env(), true),
            ["CLOUDFLARE_API_TOKEN (environment, for --cloudflare-dns)"]
        );
    }

    #[test]
    fn values_are_checked_before_anything_happens() {
        assert!(check_values(&full_args(), &full_env()).is_ok());
        let bad = |f: fn(&mut ServerCreateArgs)| {
            let mut a = full_args();
            f(&mut a);
            let e = check_values(&a, &full_env()).unwrap_err();
            assert!(e.downcast_ref::<crate::output::UsageError>().is_some());
            e.to_string()
        };
        assert!(bad(|a| a.domain = Some("https://x.com/".into())).starts_with("--domain"));
        assert!(bad(|a| a.email = Some("nope".into())).starts_with("--email"));
        assert!(bad(|a| a.name = Some("Main Server".into())).starts_with("--name"));
        assert!(bad(|a| a.install_url = Some("http://x".into())).contains("https"));
        let mut env = full_env();
        env.admin_password = Some("short".to_owned().into());
        let e = check_values(&full_args(), &env).unwrap_err().to_string();
        assert!(e.contains("MODULA_ADMIN_PASSWORD") && e.contains("10"));
    }

    #[test]
    fn secrets_in_flags_are_refused() {
        let mut a = full_args();
        a.hcloud_token = Some("x".into());
        let e = reject_secret_flags(&a).unwrap_err().to_string();
        assert!(e.contains("HCLOUD_TOKEN") && e.contains("history"));
        a.hcloud_token = None;
        a.admin_password = Some("x".into());
        assert!(reject_secret_flags(&a).is_err());
        assert!(reject_secret_flags(&full_args()).is_ok());
    }

    #[test]
    fn validators() {
        assert_eq!(
            valid_domain(" Deploy.Example.COM. ").unwrap(),
            "deploy.example.com"
        );
        for bad in [
            "example",
            "-a.com",
            "a..com",
            "a b.com",
            "a.c0m",
            "x.com/path",
        ] {
            assert!(valid_domain(bad).is_err(), "{bad}");
        }
        assert!(valid_email("Ada@Example.com").is_ok());
        assert!(valid_email("a@x").is_err());
        assert!(valid_server_name("modula-main").is_ok());
        assert!(valid_server_name("-x").is_err());
        assert!(valid_password("0123456789").is_ok());
        assert!(valid_password("012345678").is_err());
        assert!(valid_admin_name("").is_err());
    }

    #[test]
    fn ssh_keys_are_reused_or_uploaded_under_a_free_name() {
        let existing: Vec<SshKey> = serde_json::from_value(json!([
            {"id": 1, "name": "ada@laptop", "public_key": "ssh-ed25519 AAAAold ada@laptop"},
            {"id": 2, "name": "other", "public_key": "ssh-ed25519 AAAAsame different-comment"}
        ]))
        .unwrap();
        let p = Path::new("/home/ada/.ssh/id_ed25519.pub");
        assert_eq!(
            ssh_choice_for_file(p, "ssh-ed25519 AAAAsame ada@laptop".into(), &existing),
            SshChoice::Existing {
                id: 2,
                name: "other".into()
            }
        );
        match ssh_choice_for_file(p, "ssh-ed25519 AAAAnew ada@laptop".into(), &existing) {
            SshChoice::Upload { name, .. } => assert_eq!(name, "ada@laptop-2"),
            other => panic!("{other:?}"),
        }
        match ssh_choice_for_file(p, "ssh-ed25519 AAAAnew".into(), &existing) {
            SshChoice::Upload { name, .. } => assert_eq!(name, "id_ed25519.pub"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn public_key_files() {
        let dir = std::env::temp_dir().join(format!("modula-ssh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("k.pub");
        std::fs::write(&ok, "\nssh-ed25519 AAAAC3Nz you@host\n").unwrap();
        assert_eq!(
            read_public_key(&ok).unwrap(),
            "ssh-ed25519 AAAAC3Nz you@host"
        );
        let private = dir.join("k");
        std::fs::write(&private, "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        assert!(
            read_public_key(&private)
                .unwrap_err()
                .to_string()
                .contains("private key")
        );
        let junk = dir.join("j.pub");
        std::fs::write(&junk, "hello").unwrap();
        assert!(read_public_key(&junk).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
