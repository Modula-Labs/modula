//! A small Hetzner Cloud API client: just what Modula needs, shared by the
//! control plane (provisioning workers) and the `modula` CLI (creating a main
//! server from a laptop). <https://docs.hetzner.cloud/>
//!
//! The API token is only ever sent in the `Authorization` header; it is kept
//! in a [`SecretString`] (zeroized on drop) and never logged or put in errors.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::{Method, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

/// The production API.
pub const API_URL: &str = "https://api.hetzner.cloud/v1";
const USER_AGENT: &str = concat!("modula/", env!("CARGO_PKG_VERSION"));
/// Hetzner's maximum page size.
const PER_PAGE: u32 = 50;
const MAX_PAGES: u32 = 20;

/// What went wrong talking to the provider. The control plane re-exports it
/// as `provision::ProviderError`.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("the provider rejected the API token")]
    Unauthorized,
    #[error("the API token lacks permission (it needs Read & Write)")]
    Forbidden,
    #[error("not found at the provider")]
    NotFound,
    #[error("{message} ({code})")]
    Api { code: String, message: String },
    #[error("could not reach the provider: {0}")]
    Transport(String),
}

pub struct Client {
    http: reqwest::Client,
    /// Ends in `/v1`.
    api: Url,
    token: SecretString,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub network_zone: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerType {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub cores: u32,
    /// GB.
    pub memory: f64,
    /// GB.
    pub disk: u32,
    #[serde(default)]
    pub cpu_type: String,
    /// `x86` or `arm`.
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub deprecated: Option<bool>,
    #[serde(default)]
    pub deprecation: Option<Value>,
    #[serde(default)]
    pub prices: Vec<Price>,
    /// Per-location availability (newer API versions); a location with a
    /// deprecation can no longer get new servers of this type.
    #[serde(default)]
    pub locations: Vec<TypeLocation>,
}

impl ServerType {
    /// Deprecated types cannot be ordered (or soon can't): never offered.
    pub fn is_deprecated(&self) -> bool {
        self.deprecated == Some(true) || self.deprecation.as_ref().is_some_and(|d| !d.is_null())
    }

    /// Whether new servers of this type can be created in `location`.
    pub fn orderable_in(&self, location: &str) -> bool {
        if self.is_deprecated() {
            return false;
        }
        let priced = self.prices.iter().any(|p| p.location == location);
        let retired = self
            .locations
            .iter()
            .any(|l| l.name == location && l.deprecation.as_ref().is_some_and(|d| !d.is_null()));
        let listed = self.locations.is_empty() || self.locations.iter().any(|l| l.name == location);
        priced && listed && !retired
    }
}

impl ServerType {
    /// The monthly price in `location`, if it is sold there.
    pub fn monthly_price_in(&self, location: &str) -> Option<&Amount> {
        self.prices
            .iter()
            .find(|p| p.location == location)
            .map(|p| &p.price_monthly)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TypeLocation {
    pub name: String,
    #[serde(default)]
    pub deprecation: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Price {
    pub location: String,
    pub price_monthly: Amount,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Amount {
    pub net: String,
    pub gross: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SshKey {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub fingerprint: String,
    /// OpenSSH format, e.g. `ssh-ed25519 AAAA… comment`.
    #[serde(default)]
    pub public_key: String,
}

/// A datacenter in a location, with the server types it can create now.
#[derive(Debug, Clone, Deserialize)]
pub struct Datacenter {
    pub name: String,
    pub location: Location,
    #[serde(default)]
    pub server_types: DatacenterTypes,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DatacenterTypes {
    /// Ids of the types that can be created here right now (not sold out).
    #[serde(default)]
    pub available: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    /// initializing, starting, running, stopping, off, deleting, migrating, rebuilding, unknown
    pub status: String,
    #[serde(default)]
    pub public_net: Option<PublicNet>,
}

impl Server {
    pub fn ipv4(&self) -> Option<String> {
        self.public_net
            .as_ref()
            .and_then(|n| n.ipv4.as_ref())
            .map(|v| v.ip.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicNet {
    #[serde(default)]
    pub ipv4: Option<Ip>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ip {
    pub ip: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    pub id: i64,
    /// running, success, error
    pub status: String,
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Firewall {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FirewallRule {
    pub direction: &'static str,
    pub protocol: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<&'static str>,
    pub source_ips: Vec<&'static str>,
    pub description: &'static str,
}

/// What `POST /servers` needs. `root_password` in the response is never read.
#[derive(Debug, Clone, Serialize)]
pub struct CreateServer {
    pub name: String,
    pub server_type: String,
    pub location: String,
    pub image: String,
    pub ssh_keys: Vec<i64>,
    pub user_data: String,
    pub labels: BTreeMap<String, String>,
    pub firewalls: Vec<FirewallRef>,
    pub start_after_create: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FirewallRef {
    pub firewall: i64,
}

#[derive(Deserialize)]
struct CreatedServer {
    server: Server,
    #[serde(default)]
    action: Option<Action>,
}

impl Client {
    pub fn new(api: Url, token: SecretString) -> Result<Self, ProviderError> {
        // reqwest uses the process-wide rustls provider: ring, like the rest of
        // the server. Installing twice is a no-op.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        Ok(Self { http, api, token })
    }

    fn url(&self, path: &str) -> Result<Url, ProviderError> {
        let base = self.api.as_str().trim_end_matches('/');
        Url::parse(&format!("{base}{path}")).map_err(|e| ProviderError::Transport(e.to_string()))
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<T, ProviderError> {
        let mut url = self.url(path)?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let mut req = self
            .http
            .request(method, url)
            .bearer_auth(self.token.expose_secret());
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req
            .send()
            .await
            .map_err(|e| ProviderError::Transport(without_url(e)))?;
        let status = res.status();
        let bytes = res
            .bytes()
            .await
            .map_err(|e| ProviderError::Transport(without_url(e)))?;
        if status.is_success() {
            // DELETE answers 204 or an action; callers that don't care ask for `Value`.
            let bytes = if bytes.is_empty() {
                &b"null"[..]
            } else {
                &bytes
            };
            return serde_json::from_slice(bytes)
                .map_err(|e| ProviderError::Transport(format!("unexpected response: {e}")));
        }
        #[derive(Deserialize)]
        struct Envelope {
            error: ApiError,
        }
        let err = serde_json::from_slice::<Envelope>(&bytes)
            .ok()
            .map(|e| e.error);
        Err(match (status, err) {
            (StatusCode::UNAUTHORIZED, _) => ProviderError::Unauthorized,
            (_, Some(e)) if e.code == "unauthorized" => ProviderError::Unauthorized,
            (_, Some(e)) if e.code == "forbidden" => ProviderError::Forbidden,
            (StatusCode::FORBIDDEN, None) => ProviderError::Forbidden,
            (StatusCode::NOT_FOUND, _) => ProviderError::NotFound,
            (_, Some(e)) => ProviderError::Api {
                code: e.code,
                message: e.message,
            },
            (s, None) => ProviderError::Api {
                code: format!("http_{}", s.as_u16()),
                message: s.canonical_reason().unwrap_or("request failed").to_owned(),
            },
        })
    }

    /// Every page of a list endpoint.
    async fn list<T: DeserializeOwned>(
        &self,
        path: &str,
        key: &str,
        extra: &[(&str, String)],
    ) -> Result<Vec<T>, ProviderError> {
        let mut out = Vec::new();
        let mut page = 1;
        loop {
            let mut query = vec![
                ("page", page.to_string()),
                ("per_page", PER_PAGE.to_string()),
            ];
            query.extend(extra.iter().cloned());
            let mut body: Value = self.call(Method::GET, path, &query, None).await?;
            let items: Vec<T> = serde_json::from_value(body[key].take())
                .map_err(|e| ProviderError::Transport(format!("unexpected {key} list: {e}")))?;
            out.extend(items);
            let next = body["meta"]["pagination"]["next_page"].as_u64();
            match next {
                Some(n) if n > u64::from(page) && page < MAX_PAGES => page = n as u32,
                _ => return Ok(out),
            }
        }
    }

    pub async fn locations(&self) -> Result<Vec<Location>, ProviderError> {
        self.list("/locations", "locations", &[]).await
    }

    pub async fn server_types(&self) -> Result<Vec<ServerType>, ProviderError> {
        self.list("/server_types", "server_types", &[]).await
    }

    pub async fn ssh_keys(&self) -> Result<Vec<SshKey>, ProviderError> {
        self.list("/ssh_keys", "ssh_keys", &[]).await
    }

    /// Every datacenter, with the server types each can create right now.
    pub async fn datacenters(&self) -> Result<Vec<Datacenter>, ProviderError> {
        self.list("/datacenters", "datacenters", &[]).await
    }

    /// Upload a public key (OpenSSH format) under `name`.
    pub async fn create_ssh_key(
        &self,
        name: &str,
        public_key: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<SshKey, ProviderError> {
        let body = json!({ "name": name, "public_key": public_key, "labels": labels });
        let mut res: Value = self
            .call(Method::POST, "/ssh_keys", &[], Some(&body))
            .await?;
        serde_json::from_value(res["ssh_key"].take())
            .map_err(|e| ProviderError::Transport(format!("unexpected SSH key: {e}")))
    }

    /// Deleting a key that is already gone succeeds.
    pub async fn delete_ssh_key(&self, id: i64) -> Result<(), ProviderError> {
        match self
            .call::<Value>(Method::DELETE, &format!("/ssh_keys/{id}"), &[], None)
            .await
        {
            Ok(_) | Err(ProviderError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn servers(&self, label_selector: &str) -> Result<Vec<Server>, ProviderError> {
        self.list(
            "/servers",
            "servers",
            &[("label_selector", label_selector.to_owned())],
        )
        .await
    }

    pub async fn firewalls(&self, label_selector: &str) -> Result<Vec<Firewall>, ProviderError> {
        self.list(
            "/firewalls",
            "firewalls",
            &[("label_selector", label_selector.to_owned())],
        )
        .await
    }

    pub async fn create_firewall(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        rules: &[FirewallRule],
    ) -> Result<Firewall, ProviderError> {
        let body = json!({ "name": name, "labels": labels, "rules": rules });
        let mut res: Value = self
            .call(Method::POST, "/firewalls", &[], Some(&body))
            .await?;
        serde_json::from_value(res["firewall"].take())
            .map_err(|e| ProviderError::Transport(format!("unexpected firewall: {e}")))
    }

    /// Deleting a firewall that is already gone succeeds. Hetzner refuses
    /// while a server still uses it (`resource_in_use`).
    pub async fn delete_firewall(&self, id: i64) -> Result<(), ProviderError> {
        match self
            .call::<Value>(Method::DELETE, &format!("/firewalls/{id}"), &[], None)
            .await
        {
            Ok(_) | Err(ProviderError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Replace a firewall's rules (keeps an existing firewall in line with ours).
    pub async fn set_firewall_rules(
        &self,
        id: i64,
        rules: &[FirewallRule],
    ) -> Result<(), ProviderError> {
        let body = json!({ "rules": rules });
        let _: Value = self
            .call(
                Method::POST,
                &format!("/firewalls/{id}/actions/set_rules"),
                &[],
                Some(&body),
            )
            .await?;
        Ok(())
    }

    /// Attach a firewall to an existing server.
    pub async fn apply_firewall(&self, firewall: i64, server: i64) -> Result<(), ProviderError> {
        let body = json!({ "apply_to": [{ "type": "server", "server": { "id": server } }] });
        let _: Value = self
            .call(
                Method::POST,
                &format!("/firewalls/{firewall}/actions/apply_to_resources"),
                &[],
                Some(&body),
            )
            .await?;
        Ok(())
    }

    /// Returns the server and the id of its create action.
    pub async fn create_server(
        &self,
        req: &CreateServer,
    ) -> Result<(Server, Option<i64>), ProviderError> {
        let body =
            serde_json::to_value(req).map_err(|e| ProviderError::Transport(e.to_string()))?;
        let res: CreatedServer = self
            .call(Method::POST, "/servers", &[], Some(&body))
            .await?;
        Ok((res.server, res.action.map(|a| a.id)))
    }

    pub async fn server(&self, id: i64) -> Result<Server, ProviderError> {
        let mut res: Value = self
            .call(Method::GET, &format!("/servers/{id}"), &[], None)
            .await?;
        serde_json::from_value(res["server"].take())
            .map_err(|e| ProviderError::Transport(format!("unexpected server: {e}")))
    }

    /// Deleting a server that is already gone succeeds.
    pub async fn delete_server(&self, id: i64) -> Result<(), ProviderError> {
        match self
            .call::<Value>(Method::DELETE, &format!("/servers/{id}"), &[], None)
            .await
        {
            Ok(_) | Err(ProviderError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `poweron`, `poweroff`, `shutdown` or `reboot`.
    pub async fn server_action(&self, id: i64, action: &str) -> Result<Action, ProviderError> {
        let mut res: Value = self
            .call(
                Method::POST,
                &format!("/servers/{id}/actions/{action}"),
                &[],
                Some(&json!({})),
            )
            .await?;
        serde_json::from_value(res["action"].take())
            .map_err(|e| ProviderError::Transport(format!("unexpected action: {e}")))
    }

    pub async fn action(&self, id: i64) -> Result<Action, ProviderError> {
        let mut res: Value = self
            .call(Method::GET, &format!("/actions/{id}"), &[], None)
            .await?;
        serde_json::from_value(res["action"].take())
            .map_err(|e| ProviderError::Transport(format!("unexpected action: {e}")))
    }
}

/// reqwest errors include the URL; ours carry no secrets, but keep messages short.
fn without_url(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

/// `"4.5101000000"` → 452 cents, rounding any fraction of a cent up.
pub fn price_cents(amount: &str) -> Option<i64> {
    let amount = amount.trim();
    let (whole, frac) = amount.split_once('.').unwrap_or((amount, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: i64 = whole.parse().ok()?;
    let cents: i64 = format!("{:0<2}", frac.get(..2).unwrap_or(frac))
        .parse()
        .ok()?;
    let rest = frac.get(2..).unwrap_or("");
    let round_up = i64::from(rest.bytes().any(|b| b != b'0'));
    whole.checked_mul(100)?.checked_add(cents + round_up)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_round_up_to_the_cent() {
        assert_eq!(price_cents("4.5101000000"), Some(452));
        assert_eq!(price_cents("3.7900000000"), Some(379));
        assert_eq!(price_cents("12"), Some(1200));
        assert_eq!(price_cents("0.5"), Some(50));
        assert_eq!(price_cents("-1.00"), None);
        assert_eq!(price_cents("abc"), None);
    }

    #[test]
    fn deprecated_types_are_not_orderable() {
        let t: ServerType = serde_json::from_value(json!({
            "name": "cx22", "cores": 2, "memory": 4.0, "disk": 40,
            "architecture": "x86", "deprecation": null,
            "prices": [{ "location": "fsn1", "price_monthly": { "net": "3.79", "gross": "4.51" } }],
            "locations": [{ "name": "fsn1", "deprecation": null }, { "name": "nbg1", "deprecation": { "announced": "2025-01-01T00:00:00Z" } }]
        }))
        .unwrap();
        assert!(t.orderable_in("fsn1"));
        assert!(!t.orderable_in("nbg1"), "retired in that location");
        assert!(!t.orderable_in("hel1"), "no price there");
        let old: ServerType = serde_json::from_value(json!({
            "name": "cx11", "cores": 1, "memory": 2.0, "disk": 20, "deprecated": true,
            "prices": [{ "location": "fsn1", "price_monthly": { "net": "3", "gross": "3" } }]
        }))
        .unwrap();
        assert!(!old.orderable_in("fsn1"));
    }
}
