//! A project's outgoing webhooks: deployment outcomes POSTed as JSON to URLs
//! the project's members chose, signed so receivers can trust them.
//!
//! `X-Modula-Signature: sha256=<hex>` is HMAC-SHA256 of the raw body under the
//! webhook's secret (shown once at creation, stored sealed). URLs are user
//! input, so delivery is SSRF-guarded more strictly than backup endpoints:
//! loopback, link-local (cloud metadata), private and shared ranges are all
//! refused, after DNS resolution too, and redirects aren't followed.

use std::net::IpAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use modula_backup::net::GuardedResolver;
use modula_crypto::envelope::FieldAad;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use url::{Host, Url};

use crate::error::{AppError, AppResult};
use crate::jobs::{self, Job, NewJob};
use crate::state::AppState;

pub const DISPATCH_JOB: &str = "webhooks.dispatch";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    #[serde(rename = "deployment.succeeded")]
    DeploymentSucceeded,
    #[serde(rename = "deployment.failed")]
    DeploymentFailed,
    #[serde(rename = "deployment.crashed")]
    DeploymentCrashed,
    /// A replica of an active deployment crashed while others still serve.
    #[serde(rename = "deployment.degraded")]
    DeploymentDegraded,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeploymentSucceeded => "deployment.succeeded",
            Self::DeploymentFailed => "deployment.failed",
            Self::DeploymentCrashed => "deployment.crashed",
            Self::DeploymentDegraded => "deployment.degraded",
        }
    }
}

pub fn aad(workspace: Id, row: &str) -> FieldAad<'_> {
    FieldAad {
        scope: workspace,
        table: "webhooks",
        column: "secret",
        row,
    }
}

/// `https://…` or `http://…`, no credentials, a host that isn't obviously
/// internal (the resolver re-checks every address at delivery).
pub fn validate_url(raw: &str) -> AppResult<Url> {
    let bad = AppError::bad_request;
    let raw = raw.trim();
    if raw.len() > 2000 {
        return Err(bad("the URL is too long"));
    }
    let url: Url = raw
        .parse()
        .map_err(|_| bad("enter a full URL, like https://example.com/hook"))?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err(bad("the URL must start with https:// or http://"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad(
            "put credentials in the URL's path or query, not before the host",
        ));
    }
    match url.host() {
        None => return Err(bad("the URL has no host")),
        Some(Host::Ipv4(a)) if !public_ip(IpAddr::V4(a)) => {
            return Err(bad("webhooks can't target private or local addresses"));
        }
        Some(Host::Ipv6(a)) if !public_ip(IpAddr::V6(a)) => {
            return Err(bad("webhooks can't target private or local addresses"));
        }
        Some(Host::Domain(d))
            if d == "localhost" || d.ends_with(".localhost") || d.ends_with(".internal") =>
        {
            return Err(bad("webhooks can't target private or local addresses"));
        }
        _ => {}
    }
    Ok(url)
}

/// Only globally routable addresses (the shared policy, see `modula_backup::net`).
pub use modula_backup::net::public_ip;

/// Lets tests deliver to a mock server on loopback (never set otherwise).
static ALLOW_LOOPBACK: AtomicBool = AtomicBool::new(false);

pub fn allow_loopback_for_tests(allow: bool) {
    ALLOW_LOOPBACK.store(allow, Ordering::Relaxed);
}

fn allowed(ip: IpAddr) -> bool {
    public_ip(ip) || (ALLOW_LOOPBACK.load(Ordering::Relaxed) && ip.is_loopback())
}

fn http() -> anyhow::Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "modula-deploy-webhooks/",
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(std::sync::Arc::new(GuardedResolver::new(
            allowed,
            "public address",
        )))
        .no_proxy()
        .build()?;
    Ok(CLIENT.get_or_init(|| client))
}

pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret).expect("any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// What one delivery got back.
#[derive(Debug, Clone, Serialize)]
pub struct Delivery {
    pub status: Option<u16>,
    pub error: Option<String>,
}

/// POSTs a JSON `body` to a user-chosen URL through the SSRF-guarded client
/// (literal IP hosts skip the resolver, so they are checked here).
pub async fn post(url: &str, headers: &[(&str, String)], body: Vec<u8>) -> AppResult<Delivery> {
    let parsed: Url = url
        .parse()
        .map_err(|_| AppError::bad_request("invalid URL"))?;
    let literal = match parsed.host() {
        Some(Host::Ipv4(a)) => Some(IpAddr::V4(a)),
        Some(Host::Ipv6(a)) => Some(IpAddr::V6(a)),
        _ => None,
    };
    if literal.is_some_and(|ip| !allowed(ip)) {
        return Ok(Delivery {
            status: None,
            error: Some("private and local addresses are refused".into()),
        });
    }
    let mut req = http()?
        .post(parsed)
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    Ok(match req.body(body).send().await {
        Ok(r) => Delivery {
            status: Some(r.status().as_u16()),
            error: (!r.status().is_success())
                .then(|| format!("the endpoint answered {}", r.status())),
        },
        Err(e) => Delivery {
            status: None,
            error: Some(describe(&e)),
        },
    })
}

/// POSTs `body` to the webhook and records the outcome on it.
pub async fn deliver(
    state: &AppState,
    webhook: Id,
    event: &str,
    body: &serde_json::Value,
) -> AppResult<Delivery> {
    let (url, sealed, workspace): (String, Vec<u8>, Id) = sqlx::query_as(
        "SELECT w.url, w.secret_sealed, p.workspace_id FROM webhooks w
         JOIN projects p ON p.id = w.project_id WHERE w.id = $1",
    )
    .bind(webhook)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let row = webhook.to_string();
    let secret = state
        .keys
        .open(&state.db, &sealed, aad(workspace, &row))
        .await?;
    let body = serde_json::to_vec(body).map_err(anyhow::Error::from)?;
    let signature = sign(&secret, &body);
    let out = post(
        &url,
        &[
            ("x-modula-event", event.to_owned()),
            ("x-modula-delivery", Id::new().to_string()),
            ("x-modula-signature", signature),
        ],
        body,
    )
    .await?;
    sqlx::query(
        "UPDATE webhooks SET last_status = $2, last_error = $3, last_at = now() WHERE id = $1",
    )
    .bind(webhook)
    .bind(out.status.map(i32::from))
    .bind(&out.error)
    .execute(&state.db)
    .await?;
    Ok(out)
}

/// The cause without the URL (it may carry a token in its query).
fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".into()
    } else if e.is_connect() {
        let mut src: Option<&dyn std::error::Error> = std::error::Error::source(e);
        let mut last = None;
        while let Some(s) = src {
            last = Some(s.to_string());
            src = s.source();
        }
        format!("could not connect: {}", last.unwrap_or_default())
    } else if e.is_redirect() {
        "redirects aren't followed".into()
    } else {
        "the request failed".into()
    }
}

/// Queues the deliveries for a deployment's outcome (inside the transaction
/// that changes its status; the job finds the webhooks that want it).
pub async fn enqueue(conn: &mut sqlx::PgConnection, deployment: Id, event: Event) -> AppResult<()> {
    jobs::enqueue(
        conn,
        NewJob {
            kind: DISPATCH_JOB,
            payload: json!({ "deploymentId": deployment, "event": event }),
            max_attempts: Some(1),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

/// Job: delivers a deployment event to every webhook of its project that
/// wants it. A failing endpoint is recorded on its webhook, never retried
/// (receivers see each event at most once).
pub async fn dispatch_job(state: AppState, job: Job) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload {
        deployment_id: Id,
        event: Event,
    }
    let p: Payload = serde_json::from_value(job.payload)?;
    #[derive(sqlx::FromRow)]
    struct Row {
        project_id: Id,
        project: String,
        environment_id: Id,
        environment: String,
        service_id: Id,
        service: String,
        status: String,
        commit_sha: Option<String>,
        error: Option<String>,
    }
    let Some(r): Option<Row> = sqlx::query_as(
        "SELECT p.id AS project_id, p.name AS project, e.id AS environment_id, e.name AS environment,
                s.id AS service_id, s.name AS service, d.status, d.commit_sha, d.error
         FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         JOIN environments e ON e.id = i.environment_id
         WHERE d.id = $1",
    )
    .bind(p.deployment_id)
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let hooks: Vec<Id> =
        sqlx::query_scalar("SELECT id FROM webhooks WHERE project_id = $1 AND $2 = ANY(events)")
            .bind(r.project_id)
            .bind(p.event.as_str())
            .fetch_all(&state.db)
            .await?;
    if hooks.is_empty() {
        return Ok(());
    }
    let body = json!({
        "type": p.event,
        "timestamp": time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?,
        "project": { "id": r.project_id, "name": r.project },
        "environment": { "id": r.environment_id, "name": r.environment },
        "service": { "id": r.service_id, "name": r.service },
        "deployment": {
            "id": p.deployment_id,
            "status": r.status,
            "commitSha": r.commit_sha,
            "error": r.error,
        },
    });
    for h in hooks {
        if let Err(e) = deliver(&state, h, p.event.as_str(), &body).await {
            tracing::warn!(webhook = %h, error = %e, "webhook delivery failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_hmac_sha256_of_the_body() {
        // RFC 4231 test case 2.
        assert_eq!(
            sign(b"Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn internal_targets_are_refused() {
        for bad in [
            "http://127.0.0.1:7100/",
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.17.0.1/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost:5432/",
            "http://db.internal/",
            "ftp://example.com/",
            "https://user:pw@example.com/",
            "not a url",
        ] {
            assert!(validate_url(bad).is_err(), "{bad} accepted");
        }
        for good in [
            "https://discord.com/api/webhooks/1/x",
            "http://203.0.114.1/hook",
            "https://hooks.slack.com/services/x",
        ] {
            assert!(validate_url(good).is_ok(), "{good} refused");
        }
    }
}
