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

pub async fn login(
    ctx: &mut Ctx,
    url: Option<String>,
    token: Option<String>,
) -> anyhow::Result<()> {
    let url = url
        .or_else(|| ctx.cfg.url.clone())
        .ok_or_else(|| usage("pass --url https://your-instance"))?;
    let url = config::parse_url(&url)?;
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
    })?;
    let me = api.me().await?;
    ctx.cfg.url = Some(url.to_string());
    ctx.cfg.token = Some(token);
    config::save(&ctx.session.config_path, &ctx.cfg)?;
    ctx.out.either(&me, |o| {
        o.line(format!("Logged in to {url} as {}", me.user.email));
    })
}

pub fn logout(ctx: &mut Ctx) -> anyhow::Result<()> {
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
