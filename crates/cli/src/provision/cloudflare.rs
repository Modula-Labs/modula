//! Just enough of Cloudflare's v4 API to point the dashboard's domain at a
//! new server: verify the token, find the zone, look at and replace the
//! records for one name. The token lives in a [`SecretString`] (memory only)
//! and is only ever sent in the `Authorization` header.

use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

pub const API_URL: &str = "https://api.cloudflare.com/client/v4";
/// Same marker the control plane puts on the records it manages.
pub const MANAGED_COMMENT: &str = "Managed by Modula Deploy";

#[derive(Debug, Clone, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<Message>,
    result: Option<T>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    message: String,
}

pub struct Client {
    http: reqwest::Client,
    base: Url,
    token: SecretString,
}

impl Client {
    pub fn new(base: Url, token: SecretString) -> anyhow::Result<Self> {
        crate::api::install_crypto();
        let http = reqwest::Client::builder()
            .user_agent(crate::api::USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { http, base, token })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> anyhow::Result<T> {
        let mut url = Url::parse(&format!(
            "{}/{path}",
            self.base.as_str().trim_end_matches('/')
        ))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let mut req = self
            .http
            .request(method.clone(), url)
            .bearer_auth(self.token.expose_secret());
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("cannot reach Cloudflare: {}", e.without_url()))?;
        let status = res.status();
        let bytes = res.bytes().await?;
        let env: Envelope<T> = serde_json::from_slice(&bytes)
            .with_context(|| format!("unexpected response from Cloudflare (HTTP {status})"))?;
        if !status.is_success() || !env.success {
            let msg: Vec<String> = env.errors.into_iter().map(|m| m.message).collect();
            let msg = if msg.is_empty() {
                status.to_string()
            } else {
                msg.join("; ")
            };
            if matches!(status.as_u16(), 401 | 403) {
                bail!(
                    "Cloudflare refused the API token ({msg}); it needs Zone → DNS → Edit for the domain's zone"
                );
            }
            bail!("Cloudflare: {msg} (HTTP {})", status.as_u16());
        }
        env.result
            .with_context(|| format!("{method} {path}: empty response from Cloudflare"))
    }

    /// Fails unless the token is active.
    pub async fn verify(&self) -> anyhow::Result<()> {
        #[derive(Deserialize)]
        struct Status {
            status: String,
        }
        let s: Status = self
            .call(Method::GET, "user/tokens/verify", &[], None)
            .await?;
        if s.status != "active" {
            bail!("the Cloudflare API token is {}", s.status);
        }
        Ok(())
    }

    /// The active zones the token can see, by name.
    pub async fn zones(&self) -> anyhow::Result<Vec<Zone>> {
        let mut out = Vec::new();
        for page in 1..=20 {
            let page = page.to_string();
            let zones: Vec<Zone> = self
                .call(
                    Method::GET,
                    "zones",
                    &[("status", "active"), ("per_page", "50"), ("page", &page)],
                    None,
                )
                .await?;
            let done = zones.len() < 50;
            out.extend(zones);
            if done {
                break;
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// The deepest zone `host` lives in (`a.b.example.com` → `example.com`).
    pub async fn find_zone(&self, host: &str) -> anyhow::Result<Option<Zone>> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let labels: Vec<&str> = host.split('.').collect();
        for i in 0..labels.len().saturating_sub(1) {
            let candidate = labels[i..].join(".");
            let zones: Vec<Zone> = self
                .call(Method::GET, "zones", &[("name", &candidate)], None)
                .await?;
            if let Some(z) = zones
                .into_iter()
                .find(|z| z.name.eq_ignore_ascii_case(&candidate))
            {
                return Ok(Some(z));
            }
        }
        Ok(None)
    }

    /// A, AAAA and CNAME records at exactly `name`: what would compete with
    /// the A record we add.
    pub async fn address_records(&self, zone: &str, name: &str) -> anyhow::Result<Vec<DnsRecord>> {
        let all: Vec<DnsRecord> = self
            .call(
                Method::GET,
                &format!("zones/{zone}/dns_records"),
                &[("name", name), ("per_page", "100")],
                None,
            )
            .await?;
        Ok(all
            .into_iter()
            .filter(|r| matches!(r.kind.as_str(), "A" | "AAAA" | "CNAME"))
            .collect())
    }

    pub async fn delete_record(&self, zone: &str, id: &str) -> anyhow::Result<()> {
        let _: Value = self
            .call(
                Method::DELETE,
                &format!("zones/{zone}/dns_records/{id}"),
                &[],
                None,
            )
            .await?;
        Ok(())
    }

    /// A DNS-only (grey cloud) A record: Let's Encrypt's HTTP challenge and
    /// the node port (7101) need to reach the server directly.
    pub async fn create_a(&self, zone: &str, name: &str, ip: &str) -> anyhow::Result<DnsRecord> {
        let body = json!({
            "type": "A", "name": name, "content": ip, "ttl": 60, "proxied": false,
            "comment": MANAGED_COMMENT,
        });
        self.call(
            Method::POST,
            &format!("zones/{zone}/dns_records"),
            &[],
            Some(&body),
        )
        .await
    }
}
