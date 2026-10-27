//! `instance domain`: the dashboard's domain and the apps domain, set after
//! an install on the server's IP (the dashboard's Settings → Domain).

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cli::{InstanceCommand, InstanceDomainCommand};
use crate::context::Ctx;
use crate::output::Output;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainSettings {
    pub install_url: String,
    pub server_ip: Option<String>,
    pub dashboard: Option<String>,
    pub apps_domain: Option<String>,
    pub apps_domain_from_env: bool,
}

fn print(o: &Output, s: &DomainSettings) {
    o.line(format!(
        "Dashboard    {}",
        match &s.dashboard {
            Some(d) => format!("https://{d}  (also {})", s.install_url),
            None => format!("{}  (no domain yet)", s.install_url),
        }
    ));
    o.line(format!(
        "Apps domain  {}",
        match &s.apps_domain {
            Some(a) if s.apps_domain_from_env => format!("*.{a}  (MODULA_APPS_DOMAIN)"),
            Some(a) => format!("*.{a}"),
            None => "none: services use custom domains or a TCP proxy".into(),
        }
    ));
    if let Some(ip) = &s.server_ip {
        o.line(format!("Server IP    {ip}"));
    }
}

const PATH: &str = "/api/instance/domain";

pub async fn run(ctx: &Ctx, command: InstanceCommand) -> anyhow::Result<()> {
    let api = ctx.api()?;
    let InstanceCommand::Domain { command } = command;
    let current: DomainSettings = api.get(PATH).await?;
    let saved = match command {
        None => {
            ctx.out.either(&current, |o| print(o, &current))?;
            return Ok(());
        }
        Some(InstanceDomainCommand::Set {
            dashboard,
            apps_domain,
            skip_dns_check,
        }) => {
            if dashboard.is_none() && apps_domain.is_none() {
                anyhow::bail!("nothing to set: pass --dashboard and/or --apps-domain");
            }
            let body = json!({
                "dashboard": dashboard.or(current.dashboard),
                // An env-provided apps domain stays the env's.
                "appsDomain": apps_domain.or(current.apps_domain.filter(|_| !current.apps_domain_from_env)),
                "skipDnsCheck": skip_dns_check,
            });
            api.put::<_, DomainSettings>(PATH, &body).await?
        }
        Some(InstanceDomainCommand::Clear) => {
            let body = json!({
                "dashboard": null,
                "appsDomain": current.apps_domain.filter(|_| !current.apps_domain_from_env),
                "skipDnsCheck": true,
            });
            api.put::<_, DomainSettings>(PATH, &body).await?
        }
    };
    ctx.out.either(&saved, |o| {
        print(o, &saved);
        if saved.dashboard.is_some() {
            o.note(format!(
                "Certificates are issued on the first visit. This CLI keeps using {} (it stays valid).",
                saved.install_url
            ));
        }
    })?;
    Ok(())
}
