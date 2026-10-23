//! `login`, `logout`, `whoami`.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::Context as _;

use crate::api::Api;
use crate::config::{self, Login};
use crate::context::Ctx;
use crate::output::{Table, usage};

/// Read a secret from the terminal without echoing it (best effort: `stty`).
fn prompt_secret(label: &str) -> anyhow::Result<String> {
    eprint!("{label}");
    std::io::stderr().flush()?;
    let tty = std::io::stdin().is_terminal();
    let stty = |arg: &str| {
        let _ = std::process::Command::new("stty")
            .arg(arg)
            .stdin(std::process::Stdio::inherit())
            .status();
    };
    if tty {
        stty("-echo");
    }
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if tty {
        stty("echo");
        eprintln!();
    }
    read.context("cannot read the token")?;
    Ok(line.trim().to_owned())
}

/// Check `token` against the instance and save it: globally, or with
/// `local` in `<dir>/.modula/token` for this directory only.
pub async fn login(
    ctx: &mut Ctx,
    url: Option<String>,
    token: Option<String>,
    local: bool,
) -> anyhow::Result<()> {
    let url = url
        .or_else(|| ctx.saved_login().url)
        .ok_or_else(|| usage("pass --url https://your-instance"))?;
    let url = config::parse_url(&url)?;
    let access = access_for(ctx, &url).await?;
    let token = match token {
        Some(t) => t,
        None if ctx.session.interactive => prompt_secret("API token (mdt_…): ")?,
        None => return Err(usage("pass --token or set MODULA_TOKEN")),
    };
    if !token.starts_with("mdt_") {
        return Err(usage(
            "that doesn't look like an API token (they start with mdt_)",
        ));
    }
    let api = Api::new(Login {
        url: url.clone(),
        token: token.clone(),
        access: access.clone(),
    })?;
    let me = api.me().await?;
    // Only a browser sign-in is saved; a service token stays in the environment.
    let saved_access = api
        .access()
        .filter(|a| matches!(a, crate::access::CfAccess::AppToken { .. }));
    if local {
        let dir = ctx.modula_root();
        let file = save_local(ctx, &url, token, saved_access)?;
        return ctx.out.either(&me, |o| {
            o.line(format!(
                "Logged in to {url} as {} for {}",
                me.user.email,
                dir.display()
            ));
            o.note(format!("Wrote {} (readable by you only)", file.display()));
            welcome(o);
        });
    }
    ctx.cfg.url = Some(url.to_string());
    ctx.cfg.token = Some(token);
    ctx.cfg.access = saved_access;
    config::save(&ctx.session.config_path, &ctx.cfg)?;
    ctx.out.either(&me, |o| {
        o.line(format!("Logged in to {url} as {}", me.user.email));
        welcome(o);
    })
}

/// Whether `url` is behind Cloudflare Access, and the credentials to get
/// through: a service token from the environment, else a browser sign-in
/// through cloudflared.
async fn access_for(ctx: &Ctx, url: &url::Url) -> anyhow::Result<Option<crate::access::CfAccess>> {
    use crate::access::{self, CfAccess};
    let env = CfAccess::from_process_env();
    crate::api::install_crypto();
    let http = reqwest::Client::builder()
        .user_agent(crate::api::USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut req = http.get(url.join("api/health")?);
    for (k, v) in env.iter().flat_map(|a| a.headers()) {
        req = req.header(k, v);
    }
    let res = req
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    let status = res.status().as_u16();
    let location = res
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = res.bytes().await.unwrap_or_default();
    if !access::is_access_challenge(status, location.as_deref(), &body) {
        return Ok(env);
    }
    if env.is_some() {
        anyhow::bail!(
            "Cloudflare Access rejected the service token in {}/{}",
            access::ENV_CLIENT_ID,
            access::ENV_CLIENT_SECRET
        );
    }
    if !ctx.session.interactive || !access::has_cloudflared() {
        anyhow::bail!(access::help(url));
    }
    ctx.out.note(format!(
        "{url} is private (Cloudflare Access): opening your browser to sign in…"
    ));
    Ok(Some(access::browser_login(url)?))
}

/// A friendly nudge after a fresh login: what to try first (on stderr, like
/// every hint, so the result line stays the only stdout).
fn welcome(o: &crate::output::Output) {
    let next = [
        ("modula list", "see your projects"),
        (
            "modula init <name>",
            "start a project here (or `modula link`)",
        ),
        ("modula up", "deploy this directory"),
        ("modula docs", "the full reference"),
    ];
    o.note("");
    o.note(format!(
        "  {} You're in. Here's what to try next:",
        o.accent("◆")
    ));
    o.note("");
    for (cmd, what) in next {
        o.note(format!(
            "    {}  {}",
            o.accent(&format!("{cmd:<20}")),
            o.dim(what)
        ));
    }
    o.note("");
}

/// Save a checked login in `<modula_root>/.modula/token` and use it from now on.
fn save_local(
    ctx: &mut Ctx,
    url: &url::Url,
    token: String,
    access: Option<crate::access::CfAccess>,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = ctx.modula_root();
    let saved = config::Config {
        url: Some(url.to_string()),
        token: Some(token),
        access,
    };
    let file = config::save_local_login(&dir, &saved)?;
    ctx.local_login = Some((file.clone(), saved));
    ctx.ignore_modula_dir(&dir);
    Ok(file)
}

/// `link --token`: check the token and save it for this directory, quietly
/// (the link prints the result).
pub async fn save_local_token(ctx: &mut Ctx, token: String) -> anyhow::Result<()> {
    let url = ctx
        .session
        .env_url
        .clone()
        .or_else(|| ctx.saved_login().url)
        .ok_or_else(|| usage("set MODULA_URL or run `modula login --local --url …`"))?;
    let url = config::parse_url(&url)?;
    let saved_access = ctx.saved_login().access;
    let access = crate::access::CfAccess::from_process_env().or(saved_access.clone());
    Api::new(Login {
        url: url.clone(),
        token: token.clone(),
        access,
    })?
    .me()
    .await?;
    let file = save_local(ctx, &url, token, saved_access)?;
    ctx.out.note(format!(
        "Saved the token in {} (readable by you only)",
        file.display()
    ));
    Ok(())
}

/// Forget the token in effect: this directory's `.modula/token` if there is
/// one, else the global one.
pub fn logout(ctx: &mut Ctx) -> anyhow::Result<()> {
    if let Some((file, _)) = ctx.local_login.take() {
        std::fs::remove_file(&file).with_context(|| format!("cannot remove {}", file.display()))?;
        return ctx.out.either(
            &serde_json::json!({ "loggedOut": true, "file": file }),
            |o| {
                o.line(format!(
                    "Removed {}. The token itself stays valid until you delete it in the dashboard.",
                    file.display()
                ));
            },
        );
    }
    let had = ctx.cfg.token.take().is_some();
    if had {
        config::save(&ctx.session.config_path, &ctx.cfg)?;
    }
    ctx.out
        .either(&serde_json::json!({ "loggedOut": had }), |o| {
            if had {
                o.line("Logged out. The token itself stays valid until you delete it in the dashboard.");
            } else {
                o.line("Not logged in.");
            }
        })
}

pub async fn whoami(ctx: &Ctx) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let me = api.me().await?;
    ctx.out.either(&me, |o| {
        o.line(format!(
            "{} <{}> on {}",
            me.user.name, me.user.email, api.login.url
        ));
        let mut t = Table::new(&["workspace", "role", "id"]);
        for w in &me.workspaces {
            t.row([w.name.as_str(), &w.role, &w.id]);
        }
        if !t.is_empty() {
            o.table(&t);
        }
    })
}
