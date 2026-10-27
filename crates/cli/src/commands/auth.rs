//! `login` (approve in a browser; a token for CI), saved logins (`login
//! list`, `login use`, `logout`) and `whoami`.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde_json::json;
use url::Url;

use crate::api::{Api, Me};
use crate::cli::{LoginArgs, LoginCommand};
use crate::config::{self, Login, SavedLogin};
use crate::context::Ctx;
use crate::output::{Table, usage};
use crate::resolve;

/// `login`, `login list`, `login use`.
pub async fn run(
    ctx: &mut Ctx,
    args: LoginArgs,
    command: Option<LoginCommand>,
) -> anyhow::Result<()> {
    match command {
        Some(LoginCommand::List) => list(ctx),
        Some(LoginCommand::Use { name }) => use_login(ctx, name),
        None => login(ctx, args).await,
    }
}

/// Sign in to an instance and save the login: under a local name (the
/// default from now on), or with `--local` in `<dir>/.modula/token`. The
/// token comes from approving the login in a browser, or from `--token` /
/// `MODULA_TOKEN` (CI, scripts).
pub async fn login(ctx: &mut Ctx, args: LoginArgs) -> anyhow::Result<()> {
    let (url, access) = match args.url.as_deref().filter(|u| !u.trim().is_empty()) {
        Some(raw) => {
            let url = config::parse_url(raw)?;
            let access = access_for(ctx, &url).await?;
            (url, access)
        }
        None if ctx.session.interactive => ask_server(ctx).await?,
        None => {
            return Err(usage(
                "name the instance: modula login https://deploy.example.com",
            ));
        }
    };
    let name = if args.local {
        None
    } else {
        Some(login_name(ctx, &url, args.name)?)
    };

    let token = match args.token.filter(|t| !t.is_empty()) {
        Some(t) if !t.starts_with("mdt_") => {
            return Err(usage(
                "that doesn't look like an API token (they start with mdt_)",
            ));
        }
        Some(t) => t,
        None => browser_login(ctx, &url, access.clone(), args.browserless).await?,
    };
    let api = Api::new(Login {
        url: url.clone(),
        token: token.clone(),
        access,
    })?;
    let me = api.me().await?;
    // Only a browser sign-in is saved; a service token stays in the environment.
    let saved = SavedLogin {
        url: url.to_string(),
        token,
        access: api
            .access()
            .filter(|a| matches!(a, crate::access::CfAccess::AppToken { .. })),
    };
    let Some(name) = name else {
        let dir = ctx.modula_root();
        let file = save_local(ctx, saved)?;
        return ctx.out.either(&me, |o| {
            o.line(format!(
                "Logged in to {url} as {} for {}",
                me.user.email,
                dir.display()
            ));
            o.note(format!("Wrote {} (readable by you only)", file.display()));
            welcome(o);
        });
    };
    ctx.cfg.set(&name, saved);
    config::save(&ctx.session.config_path, &ctx.cfg)?;
    let shown =
        json!({ "login": name, "url": url.as_str(), "user": me.user, "workspaces": me.workspaces });
    ctx.out.either(&shown, |o| {
        o.line(format!(
            "Logged in to {url} as {} (saved as {}, now the default)",
            me.user.email,
            o.accent(&name)
        ));
        welcome(o);
    })
}

/// Ask for the instance until one answers: a domain, an IP or a URL,
/// suggesting the saved ones.
async fn ask_server(ctx: &Ctx) -> anyhow::Result<(Url, Option<crate::access::CfAccess>)> {
    let saved: Vec<String> = ctx
        .cfg
        .logins
        .values()
        .map(|l| l.url.trim_end_matches('/').to_owned())
        .collect();
    let suggest = move |input: &str| -> Result<Vec<String>, inquire::CustomUserError> {
        let input = input.trim().to_lowercase();
        Ok(saved
            .iter()
            .filter(|u| u.to_lowercase().contains(&input))
            .cloned()
            .collect())
    };
    loop {
        let raw = inquire::Text::new("Modula server (domain or IP):")
            .with_placeholder("deploy.example.com")
            .with_autocomplete(suggest.clone())
            .with_validator(|v: &str| {
                Ok(match config::parse_url(v) {
                    Ok(_) => inquire::validator::Validation::Valid,
                    Err(e) => inquire::validator::Validation::Invalid(format!("{e:#}").into()),
                })
            })
            .prompt()
            .map_err(prompt_error)?;
        let url = config::parse_url(&raw)?;
        match access_for(ctx, &url).await {
            Ok(access) => return Ok((url, access)),
            Err(e) => ctx.out.note(format!("  {e:#}; try again (Ctrl-C to quit)")),
        }
    }
}

fn prompt_error(e: inquire::InquireError) -> anyhow::Error {
    match e {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            anyhow::anyhow!("cancelled")
        }
        e => e.into(),
    }
}

/// The local name to save the login under: `--name`, else asked for at a
/// terminal (the host by default), else the host. Replacing a login for
/// another instance needs a confirmation.
fn login_name(ctx: &Ctx, url: &Url, flag: Option<String>) -> anyhow::Result<String> {
    let default = config::default_login_name(url);
    let name = match flag {
        Some(n) => n,
        None if ctx.session.interactive => inquire::Text::new("Save this login as:")
            .with_default(&default)
            .with_help_message("a local name for `--login <name>` and `modula login use`")
            .with_validator(|v: &str| {
                Ok(match config::check_login_name(v.trim()) {
                    Ok(()) => inquire::validator::Validation::Valid,
                    Err(e) => inquire::validator::Validation::Invalid(e.to_string().into()),
                })
            })
            .prompt()
            .map_err(prompt_error)?
            .trim()
            .to_owned(),
        None => default,
    };
    config::check_login_name(&name).map_err(|e| usage(e.to_string()))?;
    if let Some(old) = ctx.cfg.logins.get(&name)
        && config::parse_url(&old.url).ok().as_ref() != Some(url)
    {
        ctx.confirm(&format!(
            "The login {name} is for {}. Replace it with {url}?",
            old.url
        ))?;
    }
    Ok(name)
}

/// The browser half: start a login, show the code and the URL (opening it
/// unless `browserless` or nobody is at a terminal), and wait for the
/// approval. Returns the new API token.
async fn browser_login(
    ctx: &Ctx,
    url: &Url,
    access: Option<crate::access::CfAccess>,
    browserless: bool,
) -> anyhow::Result<String> {
    let api = Api::anonymous(url.clone(), access)?;
    let start = api.start_cli_login(&hostname()).await?;
    let o = &ctx.out;
    o.note("");
    o.note(format!(
        "  Your code: {}  (check that the browser shows the same)",
        o.accent(&start.user_code)
    ));
    let open = !browserless && ctx.session.interactive && !o.is_captured();
    if open {
        o.note(format!(
            "  Opening {} in your browser…",
            start.verification_url
        ));
        crate::commands::project::open_in_browser(&start.verification_url);
    } else {
        o.note(format!(
            "  Open this URL to approve the login: {}",
            start.verification_url
        ));
    }
    o.note("  Waiting for approval…");
    let deadline = Instant::now() + Duration::from_secs(start.expires_in);
    let mut interval = Duration::from_secs(start.interval.max(1));
    loop {
        tokio::time::sleep(interval).await;
        if Instant::now() > deadline {
            bail!("the login expired before it was approved; run `modula login` again");
        }
        let poll = match api.poll_cli_login(&start.device_code).await {
            Ok(p) => p,
            Err(e) if crate::api::api_error(&e).is_some_and(|a| a.status == 429) => {
                interval += Duration::from_secs(1);
                continue;
            }
            Err(e) => return Err(e),
        };
        match poll.status.as_str() {
            "pending" => {}
            "approved" => {
                return poll
                    .token
                    .context("the instance approved the login but sent no token");
            }
            "denied" => bail!("the login was denied in the browser"),
            _ => bail!("the login expired before it was approved; run `modula login` again"),
        }
    }
}

/// This machine's name, for the approval page and the token's name.
fn hostname() -> String {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn gethostname(name: *mut std::ffi::c_char, len: usize) -> std::ffi::c_int;
        }
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname writes at
        // most that many bytes.
        if unsafe { gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
            if !name.is_empty() {
                return name;
            }
        }
    }
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|h| !h.is_empty()))
        .unwrap_or_else(|| "unknown host".into())
}

/// Whether `url` is behind Cloudflare Access, and the credentials to get
/// through: a service token from the environment, else a browser sign-in
/// through cloudflared. Also checks that the instance answers at all.
async fn access_for(ctx: &Ctx, url: &Url) -> anyhow::Result<Option<crate::access::CfAccess>> {
    use crate::access::{self, CfAccess};
    let env = CfAccess::from_process_env();
    crate::api::install_crypto();
    let http = reqwest::Client::builder()
        .user_agent(crate::api::USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(std::sync::Arc::new(crate::api::dns::Fallback))
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
        if !(200..300).contains(&status) {
            bail!("{url} doesn't look like a Modula instance (/api/health answered {status})");
        }
        return Ok(env);
    }
    if env.is_some() {
        bail!(
            "Cloudflare Access rejected the service token in {}/{}",
            access::ENV_CLIENT_ID,
            access::ENV_CLIENT_SECRET
        );
    }
    if !ctx.session.interactive || !access::has_cloudflared() {
        bail!(access::help(url));
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
fn save_local(ctx: &mut Ctx, saved: SavedLogin) -> anyhow::Result<std::path::PathBuf> {
    let dir = ctx.modula_root();
    let file = config::save_local_login(&dir, &saved)?;
    ctx.local_login = Some((file.clone(), saved));
    ctx.ignore_modula_dir(&dir);
    Ok(file)
}

/// `link --token`: check the token and save it for this directory, quietly
/// (the link prints the result).
pub async fn save_local_token(ctx: &mut Ctx, token: String) -> anyhow::Result<()> {
    let active = ctx.active_login()?;
    let url = ctx
        .session
        .env_url
        .clone()
        .or_else(|| active.as_ref().map(|a| a.saved.url.clone()))
        .ok_or_else(|| usage("set MODULA_URL or run `modula login --local <url>`"))?;
    let url = config::parse_url(&url)?;
    let saved_access = active.and_then(|a| a.saved.access);
    let access = crate::access::CfAccess::from_process_env().or(saved_access.clone());
    Api::new(Login {
        url: url.clone(),
        token: token.clone(),
        access,
    })?
    .me()
    .await?;
    let file = save_local(
        ctx,
        SavedLogin {
            url: url.to_string(),
            token,
            access: saved_access,
        },
    )?;
    ctx.out.note(format!(
        "Saved the token in {} (readable by you only)",
        file.display()
    ));
    Ok(())
}

#[derive(Debug, serde::Serialize)]
struct LoginRow<'a> {
    name: &'a str,
    url: &'a str,
    current: bool,
}

/// `login list` / `logins`.
pub fn list(ctx: &Ctx) -> anyhow::Result<()> {
    let current = ctx.cfg.default_name();
    let rows: Vec<LoginRow> = ctx
        .cfg
        .logins
        .iter()
        .map(|(name, l)| LoginRow {
            name,
            url: &l.url,
            current: current == Some(name.as_str()),
        })
        .collect();
    ctx.out.either(&rows, |o| {
        if rows.is_empty() {
            o.line("No saved logins: run `modula login`.");
        } else {
            let mut t = Table::new(&["", "name", "url"]);
            for r in &rows {
                t.row([if r.current { "*" } else { "" }, r.name, r.url]);
            }
            o.table(&t);
        }
        if let Some((file, l)) = &ctx.local_login {
            o.note(format!(
                "This directory uses {} from {}.",
                l.url,
                file.display()
            ));
        }
    })
}

/// `login use [<name>]`: make a saved login the default.
pub fn use_login(ctx: &mut Ctx, name: Option<String>) -> anyhow::Result<()> {
    let name = match name {
        Some(n) => n,
        None if resolve::interactive() => {
            let logins: Vec<(&String, &SavedLogin)> = ctx.cfg.logins.iter().collect();
            if logins.is_empty() {
                bail!("no saved logins: run `modula login`");
            }
            resolve::choose("Use which login?", &logins, |(n, l)| login_label(n, l))?
                .0
                .clone()
        }
        None => {
            return Err(usage(format!(
                "name the login to use (logins: {})",
                ctx.cfg.names()
            )));
        }
    };
    let Some(l) = ctx.cfg.logins.get(&name).cloned() else {
        bail!("no login named {name} (logins: {})", ctx.cfg.names());
    };
    ctx.cfg.current = Some(name.clone());
    config::save(&ctx.session.config_path, &ctx.cfg)?;
    ctx.out
        .either(&json!({ "login": name, "url": l.url }), |o| {
            o.line(format!("Now using {name} ({})", l.url))
        })
}

/// `work  https://deploy.example.com/`.
pub fn login_label(name: &str, l: &SavedLogin) -> String {
    format!("{name}  {}", l.url)
}

const STILL_VALID: &str =
    "The token itself stays valid until you delete it in the dashboard (Account → API tokens).";

/// `logout [<name>|--all]`: forget a saved login. Without a name: this
/// directory's `.modula/token` if any, else the login in use.
pub fn logout(ctx: &mut Ctx, name: Option<String>, all: bool) -> anyhow::Result<()> {
    if all {
        let names: Vec<String> = ctx.cfg.logins.keys().cloned().collect();
        ctx.cfg = config::Config::default();
        config::save(&ctx.session.config_path, &ctx.cfg)?;
        return ctx.out.either(&json!({ "loggedOut": names }), |o| {
            if names.is_empty() {
                o.line("No saved logins.");
            } else {
                o.line(format!("Forgot {}. {STILL_VALID}", names.join(", ")));
            }
        });
    }
    if name.is_none()
        && ctx.global.login.is_none()
        && let Some((file, _)) = ctx.local_login.take()
    {
        std::fs::remove_file(&file).with_context(|| format!("cannot remove {}", file.display()))?;
        return ctx
            .out
            .either(&json!({ "loggedOut": true, "file": file }), |o| {
                o.line(format!("Removed {}. {STILL_VALID}", file.display()))
            });
    }
    let name = match name {
        Some(n) => Some(n),
        None => match ctx.active_login().ok().flatten().and_then(|a| a.name) {
            Some(n) => Some(n),
            None if resolve::interactive() && !ctx.cfg.logins.is_empty() => {
                let logins: Vec<(&String, &SavedLogin)> = ctx.cfg.logins.iter().collect();
                Some(
                    resolve::choose("Log out of which login?", &logins, |(n, l)| {
                        login_label(n, l)
                    })?
                    .0
                    .clone(),
                )
            }
            None if ctx.cfg.logins.is_empty() => None,
            None => {
                return Err(usage(format!(
                    "name the login to forget (logins: {})",
                    ctx.cfg.names()
                )));
            }
        },
    };
    let Some(name) = name else {
        return ctx
            .out
            .either(&json!({ "loggedOut": false }), |o| o.line("Not logged in."));
    };
    if !ctx.cfg.remove(&name) {
        bail!("no login named {name} (logins: {})", ctx.cfg.names());
    }
    config::save(&ctx.session.config_path, &ctx.cfg)?;
    let next = ctx.cfg.default_name().map(str::to_owned);
    ctx.out
        .either(&json!({ "loggedOut": name, "current": next }), |o| {
            o.line(format!("Forgot the login {name}. {STILL_VALID}"));
            match &next {
                Some(n) => o.note(format!("The default login is now {n}.")),
                None if !ctx.cfg.logins.is_empty() => {
                    o.note("Pick the default login with `modula login use`.")
                }
                None => {}
            }
        })
}

pub async fn whoami(ctx: &Ctx) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let me: Me = api.me().await?;
    let active = if ctx.session.env_token.is_some() {
        None
    } else {
        ctx.active_login()?
    };
    let source = match &active {
        Some(a) => match (&a.name, &a.file) {
            (Some(n), _) => n.clone(),
            (None, Some(f)) => f.display().to_string(),
            (None, None) => String::new(),
        },
        None => "MODULA_TOKEN".into(),
    };
    let shown = json!({
        "login": active.as_ref().and_then(|a| a.name.clone()),
        "url": api.login.url.as_str(),
        "user": me.user,
        "workspaces": me.workspaces,
    });
    ctx.out.either(&shown, |o| {
        o.line(format!(
            "{} <{}> on {} (login: {source})",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_has_a_name() {
        let h = hostname();
        assert!(!h.is_empty() && !h.contains('\0'), "{h:?}");
    }

    #[test]
    fn logins_are_labelled_with_their_url() {
        let l = SavedLogin {
            url: "https://deploy.example.com/".into(),
            token: "mdt_x".into(),
            access: None,
        };
        assert_eq!(login_label("work", &l), "work  https://deploy.example.com/");
    }
}
