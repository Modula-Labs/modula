//! Custom domains: hostname rules, the DNS records a user must create, and
//! ownership verification (by hand through "Check now", and in the background).
//!
//! A custom domain is only routed once verified. Nodes derive their `ask`
//! allowlist from the routes they get, so an unverified domain never gets a
//! certificate either.

pub mod dns;
pub mod provider;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use modula_id::Id;
use serde::Serialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::config::Config;
use crate::deploy;
use crate::error::AppResult;
use crate::http::meta::RequestMeta;
use crate::state::AppState;

/// Ownership proof lives in a TXT record at `<CHALLENGE_LABEL>.<hostname>`.
pub const CHALLENGE_LABEL: &str = "_modula-challenge";
/// The background verifier's tick, and how long it keeps retrying a domain
/// that never verifies ("Check now" keeps working after that).
const VERIFY_EVERY: Duration = Duration::from_secs(60);
const GIVE_UP_AFTER_DAYS: i32 = 7;

/// A validated custom hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hostname {
    /// Lowercase, no trailing dot; `*.example.com` for wildcards.
    pub name: String,
    pub wildcard: bool,
}

impl Hostname {
    /// The name without a leading `*.`.
    pub fn base(&self) -> &str {
        self.name.strip_prefix("*.").unwrap_or(&self.name)
    }
}

/// Validates a user-entered custom domain. Pasted URLs (`https://x.com/`) are
/// accepted; internationalized names must be entered in punycode.
pub fn parse_custom(
    raw: &str,
    cfg: &Config,
    hosts: &crate::instance_domain::Hosts,
) -> Result<Hostname, String> {
    let mut h = raw.trim();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = h.strip_prefix(scheme) {
            h = rest;
        }
    }
    let h = h.trim_end_matches('/').trim_end_matches('.').to_owned();
    if h.is_empty() {
        return Err("enter a domain like app.example.com".into());
    }
    if !h.is_ascii() {
        return Err("use the punycode form (xn--…) for internationalized domains".into());
    }
    let h = h.to_ascii_lowercase();
    if h.len() > 253 {
        return Err("a domain can be at most 253 characters".into());
    }
    if h.parse::<IpAddr>().is_ok() || h.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
        return Err("enter a domain name, not an IP address".into());
    }
    if h.contains(['/', ':', '?', '#', '@']) {
        return Err("enter just the domain, without a path or port".into());
    }
    let (wildcard, base) = match h.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, h.as_str()),
    };
    // The edge doesn't route wildcard names yet (they'd need DNS-01
    // certificates through a DNS provider); refuse rather than half-work.
    if wildcard {
        return Err(
            "wildcard domains aren't supported yet (they need a DNS provider for \
             certificates); add each name separately"
                .into(),
        );
    }
    let labels: Vec<&str> = base.split('.').collect();
    if labels.len() < 2 {
        return Err("enter a full domain like app.example.com".into());
    }
    for l in &labels {
        if !crate::validate::dns_label(l) {
            return Err(format!(
                "{l:?} is not a valid DNS label: use letters, digits and hyphens"
            ));
        }
    }
    let tld = labels[labels.len() - 1];
    if tld.bytes().all(|b| b.is_ascii_digit()) {
        return Err("enter a domain name, not an IP address".into());
    }
    for reserved in ["localhost", "local", "internal", "invalid", "arpa"] {
        if tld == reserved {
            return Err(format!(".{reserved} names can't get public certificates"));
        }
    }
    let under = |parent: &str| base == parent || base.ends_with(&format!(".{parent}"));
    if let Some(apps) = hosts.apps.as_deref()
        && under(apps)
    {
        return Err(format!(
            "names under {apps} are generated domains; use \"Generate domain\" instead"
        ));
    }
    let dashboards = [
        cfg.public_url.host_str().map(str::to_ascii_lowercase),
        hosts.dashboard.clone(),
    ];
    if dashboards.iter().flatten().any(|d| under(d)) {
        return Err("that name belongs to this dashboard".into());
    }
    if cfg.tcp_proxy_host.as_deref() == Some(base) {
        return Err("that name is this instance's TCP proxy host".into());
    }
    Ok(Hostname {
        name: h.clone(),
        wildcard,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DnsRecord {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: String,
    pub value: String,
}

/// Where a custom domain should point: the service's generated domain
/// (CNAME) or, for apex names and services without one, its servers' IPs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Target {
    pub cname: Option<String>,
    pub ips: Vec<String>,
}

pub async fn target(db: &sqlx::PgPool, instance: Id) -> AppResult<Target> {
    let cname = sqlx::query_scalar::<_, String>(
        "SELECT hostname FROM domains WHERE service_instance_id = $1 AND kind = 'generated'
         ORDER BY created_at LIMIT 1",
    )
    .bind(instance)
    .fetch_optional(db)
    .await?;
    let ips: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT n.public_ip FROM nodes n
         WHERE n.public_ip IS NOT NULL AND n.status <> 'revoked' AND (
             n.id IN (SELECT node_id FROM deployments
                      WHERE service_instance_id = $1 AND status IN ('active', 'crashed'))
             OR n.id = (SELECT node_id FROM service_instances WHERE id = $1))
         ORDER BY n.public_ip",
    )
    .bind(instance)
    .fetch_all(db)
    .await?;
    Ok(Target { cname, ips })
}

/// Apex names (`example.com`) can't have a CNAME next to their SOA/NS records.
/// Two labels is a heuristic: `example.co.uk` is an apex too, and gets a CNAME
/// suggestion its DNS host may only accept as ALIAS/flattened CNAME.
fn is_apex(base: &str) -> bool {
    base.split('.').count() == 2
}

/// The records a user creates for `hostname`: routing (CNAME or A) and the
/// TXT ownership proof.
pub fn required_records(hostname: &str, token: &str, target: &Target) -> Vec<DnsRecord> {
    let base = hostname.strip_prefix("*.").unwrap_or(hostname);
    let mut records = Vec::new();
    match (&target.cname, is_apex(base)) {
        (Some(cname), false) => records.push(DnsRecord {
            kind: "CNAME",
            name: hostname.to_owned(),
            value: cname.clone(),
        }),
        _ => records.extend(target.ips.iter().map(|ip| DnsRecord {
            kind: "A",
            name: hostname.to_owned(),
            value: ip.clone(),
        })),
    }
    records.push(DnsRecord {
        kind: "TXT",
        name: format!("{CHALLENGE_LABEL}.{base}"),
        value: token.to_owned(),
    });
    records
}

/// A fresh verification token (`modula-verify=<32 hex>`).
pub fn new_token() -> anyhow::Result<String> {
    Ok(format!(
        "modula-verify={}",
        hex::encode(modula_crypto::random_bytes::<16>()?)
    ))
}

/// The workspace's DNS provider, if one is configured: Cloudflare when the
/// workspace connected it (`integrations/cloudflare`).
pub async fn provider_for(
    state: &AppState,
    workspace: Id,
) -> Option<Box<dyn provider::DnsProvider>> {
    match crate::integrations::cloudflare::connection(state, workspace).await {
        Ok(Some(c)) => Some(Box::new(
            crate::integrations::cloudflare::dns::CloudflareDns::new(c.client),
        )),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(%workspace, error = ?e, "cannot load the Cloudflare integration");
            None
        }
    }
}

/// Writes `records` at the workspace's DNS provider when it manages the zone.
/// Best effort: returns a note for the user instead of failing the request.
pub async fn sync_records(
    state: &AppState,
    workspace: Id,
    hostname: &str,
    records: &[DnsRecord],
    remove: bool,
    proxied: bool,
) -> Option<String> {
    let p = provider_for(state, workspace).await?;
    let zone = match p.find_zone(hostname).await {
        Ok(Some(z)) => z,
        Ok(None) => return None,
        Err(e) => return Some(format!("{}: {e:#}", p.name())),
    };
    let routing: Vec<(&str, &str)> = records
        .iter()
        .filter(|r| r.kind != "TXT")
        .map(|r| (r.kind, r.value.as_str()))
        .collect();
    if !remove
        && !routing.is_empty()
        && let Err(e) = p.set_routing(&zone, hostname, &routing, proxied).await
    {
        tracing::warn!(
            hostname,
            error = format!("{e:#}"),
            "DNS provider sync failed"
        );
        return Some(format!("{} could not update {hostname}: {e:#}", p.name()));
    }
    for r in records.iter().filter(|r| remove || r.kind == "TXT") {
        let res = if remove {
            p.delete_record(&zone, &r.name, r.kind).await
        } else {
            p.ensure_record(&zone, &r.name, r.kind, &r.value, false)
                .await
        };
        if let Err(e) = res {
            tracing::warn!(
                hostname,
                error = format!("{e:#}"),
                "DNS provider sync failed"
            );
            return Some(format!(
                "{} could not update {} {}: {e:#}",
                p.name(),
                r.kind,
                r.name
            ));
        }
    }
    None
}

pub const RESYNC_JOB: &str = "domains.resync_routing";

/// Job: rewrites the routing records of an instance's custom domains at the
/// workspace's DNS provider after it moved to other servers, so A records
/// follow it (a server's IP is dropped once its deployment there is gone).
pub async fn resync_job(state: AppState, job: crate::jobs::Job) -> anyhow::Result<()> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload {
        service_instance_id: Id,
    }
    let p: Payload = serde_json::from_value(job.payload)?;
    let rows: Vec<(String, String, bool, Id)> = sqlx::query_as(
        "SELECT m.hostname, COALESCE(m.verification_token, ''), m.proxied, p.workspace_id
         FROM domains m
         JOIN service_instances i ON i.id = m.service_instance_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         WHERE m.service_instance_id = $1 AND m.kind = 'custom'",
    )
    .bind(p.service_instance_id)
    .fetch_all(&state.db)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let t = target(&state.db, p.service_instance_id).await?;
    for (hostname, token, proxied, workspace) in rows {
        let records = required_records(&hostname, &token, &t);
        if let Some(note) =
            sync_records(&state, workspace, &hostname, &records, false, proxied).await
        {
            anyhow::bail!(note);
        }
    }
    Ok(())
}

/// The resolver verification uses (swappable for tests).
pub fn resolver(state: &AppState) -> Arc<dyn dns::Resolver> {
    crate::sync::read(&state.dns).clone()
}

/// Outcome of one ownership check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: &'static str,
    pub error: Option<String>,
    pub target_ok: Option<bool>,
}

/// Looks up the TXT proof and whether the name points at us. A failed lookup
/// leaves the status as it was.
pub async fn evaluate(
    dns: &dyn dns::Resolver,
    hostname: &str,
    token: &str,
    target: &Target,
    current: &str,
) -> Outcome {
    let base = hostname.strip_prefix("*.").unwrap_or(hostname);
    let challenge = format!("{CHALLENGE_LABEL}.{base}");
    let (status, error) = match dns.txt(&challenge).await {
        Ok(txts) if txts.iter().any(|t| t.trim() == token) => ("verified", None),
        Ok(txts) if txts.is_empty() => (
            if current == "failed" {
                "failed"
            } else {
                "pending"
            },
            Some(format!("No TXT record found at {challenge} yet")),
        ),
        Ok(_) => (
            "failed",
            Some(format!(
                "The TXT record at {challenge} does not contain the expected value"
            )),
        ),
        Err(e) => (
            if current == "verified" {
                "verified"
            } else {
                "pending"
            },
            Some(format!("DNS lookup failed: {e}")),
        ),
    };
    // Wildcards can't be resolved as such; check a name under them instead.
    let probe = if hostname.starts_with("*.") {
        format!("modula-probe.{base}")
    } else {
        hostname.to_owned()
    };
    let mut target_ok = None;
    if let Some(cname) = &target.cname
        && let Ok(found) = dns.cname(&probe).await
        && found.iter().any(|c| c == cname)
    {
        target_ok = Some(true);
    }
    if target_ok.is_none() && (target.cname.is_some() || !target.ips.is_empty()) {
        // A CNAME to us resolves to our IPs too, so this also covers
        // flattened records, as long as the edge isn't proxied.
        let ours: Vec<Ipv4Addr> = target.ips.iter().filter_map(|i| i.parse().ok()).collect();
        target_ok = match dns.ipv4(&probe).await {
            Ok(found) => Some(!ours.is_empty() && found.iter().any(|ip| ours.contains(ip))),
            Err(_) => None,
        };
    }
    Outcome {
        status,
        error,
        target_ok,
    }
}

/// Bumps the desired state of the nodes serving `instance` (its routes changed)
/// and returns them, to kick once the transaction commits.
pub async fn bump_serving_nodes(tx: &mut sqlx::PgConnection, instance: Id) -> AppResult<Vec<Id>> {
    let nodes: Vec<Option<Id>> = sqlx::query_scalar(
        "SELECT DISTINCT node_id FROM deployments
         WHERE service_instance_id = $1 AND status IN ('active', 'crashed')",
    )
    .bind(instance)
    .fetch_all(&mut *tx)
    .await?;
    let nodes: Vec<Id> = nodes.into_iter().flatten().collect();
    for n in &nodes {
        deploy::bump(&mut *tx, *n).await?;
    }
    Ok(nodes)
}

#[derive(sqlx::FromRow)]
struct CheckRow {
    service_instance_id: Id,
    hostname: String,
    verification_token: Option<String>,
    status: String,
    workspace_id: Id,
}

/// Checks one custom domain and stores the result. Routing follows the
/// status, so a change bumps the serving nodes. `actor` is None for the
/// background verifier.
pub async fn check(
    state: &AppState,
    id: Id,
    actor: Option<(&Actor, &RequestMeta)>,
) -> AppResult<()> {
    let Some(row): Option<CheckRow> = sqlx::query_as(
        "SELECT m.service_instance_id, m.hostname, m.verification_token, m.status, p.workspace_id
         FROM domains m
         JOIN service_instances i ON i.id = m.service_instance_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         WHERE m.id = $1 AND m.kind = 'custom'",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let token = row.verification_token.unwrap_or_default();
    let target = target(&state.db, row.service_instance_id).await?;
    let dns = resolver(state);
    let out = evaluate(&*dns, &row.hostname, &token, &target, &row.status).await;

    let mut tx = state.db.begin().await?;
    // Only from the status we read, so a concurrent check or delete wins cleanly.
    let updated = sqlx::query(
        "UPDATE domains SET status = $3, last_error = $4, dns_target_ok = $5, checked_at = now(),
                verified_at = CASE WHEN $3 = 'verified' THEN COALESCE(verified_at, now()) ELSE NULL END
         WHERE id = $1 AND status = $2",
    )
    .bind(id)
    .bind(&row.status)
    .bind(out.status)
    .bind(&out.error)
    .bind(out.target_ok)
    .execute(&mut *tx)
    .await?;
    let changed = updated.rows_affected() == 1 && out.status != row.status;
    let mut nodes = Vec::new();
    if changed {
        nodes = bump_serving_nodes(&mut tx, row.service_instance_id).await?;
        let system = RequestMeta {
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            user_agent: Some("modula-server/domain-verifier".into()),
        };
        let meta = actor.map_or(&system, |(_, m)| m);
        let mut entry = Entry::new(
            if out.status == "verified" {
                "domain.verify"
            } else {
                "domain.unverify"
            },
            meta,
        )
        .workspace(row.workspace_id)
        .target("domain", id)
        .metadata(json!({ "hostname": row.hostname, "status": out.status, "error": out.error }));
        if let Some((a, _)) = actor {
            entry = entry.actor(a);
        }
        entry.record(&mut *tx).await?;
    }
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(())
}

/// Re-checks unverified custom domains every minute for a week.
pub async fn verify_loop(state: AppState, stop: CancellationToken) {
    let mut tick = tokio::time::interval(VERIFY_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        let due: Vec<Id> = match sqlx::query_scalar(
            "SELECT id FROM domains
             WHERE kind = 'custom' AND status <> 'verified'
               AND created_at > now() - make_interval(days => $1)
               AND (checked_at IS NULL OR checked_at < now() - interval '50 seconds')
             ORDER BY checked_at NULLS FIRST LIMIT 200",
        )
        .bind(GIVE_UP_AFTER_DAYS)
        .fetch_all(&state.db)
        .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "could not list domains to verify");
                continue;
            }
        };
        for id in due {
            if stop.is_cancelled() {
                return;
            }
            if let Err(e) = check(&state, id, None).await {
                tracing::warn!(domain = %id, error = ?e, "domain check failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dns::{RecordKind, StaticDns};

    fn cfg() -> Config {
        let mut c = Config::for_tests("https://deploy.example.com");
        c.apps_domain = Some("apps.example.com".into());
        c
    }

    #[test]
    fn hostnames() {
        let c = cfg();
        let ok = |s: &str| {
            parse_custom(s, &c, &crate::instance_domain::Hosts::from_config(&c)).map(|h| h.name)
        };
        assert_eq!(ok(" WWW.Shop.com. ").unwrap(), "www.shop.com");
        assert_eq!(ok("https://shop.com/").unwrap(), "shop.com");
        assert_eq!(ok("xn--bcher-kva.de").unwrap(), "xn--bcher-kva.de");
        // Wildcards aren't routed by the edge yet.
        let wc = parse_custom(
            "*.shop.com",
            &c,
            &crate::instance_domain::Hosts::from_config(&c),
        )
        .unwrap_err();
        assert!(wc.contains("wildcard"), "{wc}");
        // The dashboard's parent is fine; the dashboard itself is not.
        assert_eq!(ok("example.com").unwrap(), "example.com");
        for bad in [
            "",
            "localhost",
            "shop",
            "1.2.3.4",
            "[::1]",
            "::1",
            "bücher.de",
            "a..com",
            "-a.com",
            "a-.com",
            "a_b.com",
            "shop.com:443",
            "shop.com/path",
            "*.*.shop.com",
            "a.*.shop.com",
            "*",
            "*.com.",
            "x.local",
            "svc.modula.internal",
            "apps.example.com",
            "api.apps.example.com",
            "deploy.example.com",
            "x.deploy.example.com",
            "1.2.3.400",
        ] {
            assert!(
                parse_custom(bad, &c, &crate::instance_domain::Hosts::from_config(&c)).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(ok(&format!("{}.com", "a".repeat(64))).is_err());
        assert!(ok(&format!("{}.com", "a".repeat(63))).is_ok());
    }

    #[test]
    fn records() {
        let t = Target {
            cname: Some("api-x1y2.apps.example.com".into()),
            ips: vec!["203.0.113.7".into()],
        };
        let r = required_records("www.shop.com", "tok", &t);
        assert_eq!(r[0].kind, "CNAME");
        assert_eq!(r[0].value, "api-x1y2.apps.example.com");
        assert_eq!(r[1].kind, "TXT");
        assert_eq!(r[1].name, "_modula-challenge.www.shop.com");
        // Apex names get A records.
        let r = required_records("shop.com", "tok", &t);
        assert_eq!((r[0].kind, r[0].value.as_str()), ("A", "203.0.113.7"));
        // Without a generated domain, A records for subdomains too.
        let r = required_records("www.shop.com", "tok", &Target::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].kind, "TXT");
    }

    #[tokio::test]
    async fn verification_outcomes() {
        let dns = StaticDns::default();
        let t = Target {
            cname: Some("api.apps.example.com".into()),
            ips: vec!["203.0.113.7".into()],
        };
        let o = evaluate(&dns, "www.shop.com", "tok", &t, "pending").await;
        assert_eq!(o.status, "pending");
        assert!(o.error.unwrap().contains("No TXT record"));
        assert_eq!(o.target_ok, Some(false));

        dns.set(
            RecordKind::Txt,
            "_modula-challenge.www.shop.com",
            &["other"],
        );
        let o = evaluate(&dns, "www.shop.com", "tok", &t, "pending").await;
        assert_eq!(o.status, "failed");

        dns.set(
            RecordKind::Txt,
            "_modula-challenge.www.shop.com",
            &["x", " tok "],
        );
        dns.set(RecordKind::Cname, "www.shop.com", &["api.apps.example.com"]);
        let o = evaluate(&dns, "www.shop.com", "tok", &t, "failed").await;
        assert_eq!(o.status, "verified");
        assert_eq!(o.error, None);
        assert_eq!(o.target_ok, Some(true));

        // Proxied apex: TXT is enough, the A record is just reported.
        dns.set(RecordKind::Txt, "_modula-challenge.shop.com", &["tok"]);
        dns.set(RecordKind::A, "shop.com", &["104.16.0.1"]);
        let o = evaluate(&dns, "shop.com", "tok", &t, "pending").await;
        assert_eq!((o.status, o.target_ok), ("verified", Some(false)));
    }
}
