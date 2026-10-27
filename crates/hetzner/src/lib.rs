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
    /// `local` or `network`: a server can only change to a type with the same.
    #[serde(default)]
    pub storage_type: String,
    #[serde(default)]
    pub deprecated: Option<bool>,
    #[serde(default)]
    pub deprecation: Option<Value>,
    #[serde(default)]
    pub prices: Vec<Price>,
    /// Per-location availability: `available: false` is sold out there, a
    /// deprecation means retired there. (Hetzner's API has no
    /// `GET /datacenters` since 2026-10-01; this is where availability lives.)
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
        let here = self.locations.iter().find(|l| l.name == location);
        let retired = here.is_some_and(|l| l.deprecation.as_ref().is_some_and(|d| !d.is_null()));
        let sold_out = here.is_some_and(|l| l.available == Some(false));
        let listed = self.locations.is_empty() || here.is_some();
        priced && listed && !retired && !sold_out
    }
}

impl ServerType {
    /// The monthly price in `location`, if it is sold there.
    /// Outgoing traffic included per month in `location`, in bytes.
    pub fn included_traffic_in(&self, location: &str) -> Option<u64> {
        self.prices
            .iter()
            .find(|p| p.location == location)
            .and_then(|p| p.included_traffic)
    }

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
    /// False while sold out in this location.
    #[serde(default)]
    pub available: Option<bool>,
    #[serde(default)]
    pub recommended: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Price {
    pub location: String,
    pub price_monthly: Amount,
    /// Outgoing traffic included per month, in bytes.
    #[serde(default)]
    pub included_traffic: Option<u64>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    pub id: i64,
    pub name: String,
    /// initializing, starting, running, stopping, off, deleting, migrating, rebuilding, unknown
    pub status: String,
    #[serde(default)]
    pub public_net: Option<PublicNet>,
    /// The server's current type (possibly one no longer sold).
    #[serde(default)]
    pub server_type: Option<ServerType>,
    #[serde(default)]
    pub location: Option<Location>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// GB. The disk it really has: smaller than its type's after a change
    /// to a bigger type that kept the disk (`upgrade_disk: false`).
    #[serde(default)]
    pub primary_disk_size: Option<u32>,
}

impl Server {
    pub fn ipv4(&self) -> Option<String> {
        self.public_net
            .as_ref()
            .and_then(|n| n.ipv4.as_ref())
            .map(|v| v.ip.clone())
    }

    /// The disk it has now (its type's when Hetzner doesn't say).
    pub fn disk_gb(&self) -> Option<u32> {
        self.primary_disk_size
            .or_else(|| self.server_type.as_ref().map(|t| t.disk))
    }

    /// Why `change_type` to `to` (keeping the disk, `upgrade_disk: false`)
    /// would be refused, or None when it can be done. Hetzner's rules: the
    /// same storage type, a disk at least as big as the current one (disks
    /// never shrink), and the same CPU architecture (the installed OS is
    /// built for it).
    pub fn change_type_blocker(&self, to: &ServerType) -> Option<String> {
        let current = self.server_type.as_ref()?;
        if !current.architecture.is_empty()
            && !to.architecture.is_empty()
            && current.architecture != to.architecture
        {
            return Some(format!(
                "{} is {}, this server is {}: the operating system can't move between architectures",
                to.name,
                arch_name(&to.architecture),
                arch_name(&current.architecture)
            ));
        }
        if !current.storage_type.is_empty()
            && !to.storage_type.is_empty()
            && current.storage_type != to.storage_type
        {
            return Some(format!(
                "{} has {} storage, this server {}",
                to.name, to.storage_type, current.storage_type
            ));
        }
        if let Some(disk) = self.disk_gb()
            && to.disk < disk
        {
            return Some(format!(
                "{} has a {} GB disk, smaller than this server's {disk} GB (disks can't shrink)",
                to.name, to.disk
            ));
        }
        None
    }
}

fn arch_name(arch: &str) -> &str {
    match arch {
        "x86" => "x86 (Intel/AMD)",
        "arm" => "Arm",
        other => other,
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

/// The description of the firewall rule that lets Modula's servers reach
/// each other's WireGuard port; the control plane rewrites it as servers
/// come and go, and whoever else sets a Modula firewall's rules keeps it.
pub const MESH_RULE: &str = "Modula private network";

#[derive(Debug, Clone, Deserialize)]
pub struct Firewall {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub rules: Vec<FirewallRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallRule {
    pub direction: String,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_ips: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destination_ips: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl FirewallRule {
    /// Inbound `protocol` (`port`, a number or range) from anywhere.
    pub fn inbound_any(protocol: &str, port: Option<&str>, description: &str) -> Self {
        Self::inbound(
            protocol,
            port,
            vec!["0.0.0.0/0".into(), "::/0".into()],
            description,
        )
    }

    /// Inbound `protocol` (`port`) from `sources` (CIDRs; never empty: the
    /// API refuses an inbound rule without sources).
    pub fn inbound(
        protocol: &str,
        port: Option<&str>,
        sources: Vec<String>,
        description: &str,
    ) -> Self {
        Self {
            direction: "in".into(),
            protocol: protocol.into(),
            port: port.map(str::to_owned),
            source_ips: sources,
            destination_ips: Vec::new(),
            description: Some(description.into()),
        }
    }
}

/// What `POST /servers` needs. `root_password` in the response is never read.
/// `Debug` leaves `user_data` out: it holds a one-time setup or join token.
#[derive(Clone, Serialize)]
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

impl std::fmt::Debug for CreateServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateServer")
            .field("name", &self.name)
            .field("server_type", &self.server_type)
            .field("location", &self.location)
            .field("image", &self.image)
            .field("ssh_keys", &self.ssh_keys)
            .field(
                "user_data",
                &format_args!("<redacted, {} bytes>", self.user_data.len()),
            )
            .field("labels", &self.labels)
            .field("firewalls", &self.firewalls)
            .field("start_after_create", &self.start_after_create)
            .finish()
    }
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

    /// Change a powered-off server's type. With `upgrade_disk: false` the disk
    /// keeps its size, so the server can be downsized again later.
    pub async fn change_server_type(
        &self,
        id: i64,
        server_type: &str,
        upgrade_disk: bool,
    ) -> Result<Action, ProviderError> {
        let body = json!({ "server_type": server_type, "upgrade_disk": upgrade_disk });
        let mut res: Value = self
            .call(
                Method::POST,
                &format!("/servers/{id}/actions/change_type"),
                &[],
                Some(&body),
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
    fn create_server_debug_hides_user_data() {
        let req = CreateServer {
            name: "modula-main".into(),
            server_type: "cx22".into(),
            location: "fsn1".into(),
            image: "ubuntu-24.04".into(),
            ssh_keys: vec![7],
            user_data: "#!/bin/bash\nbash install.sh --setup-token mcsetup_SECRET\n".into(),
            labels: BTreeMap::from([("modula".into(), "main".into())]),
            firewalls: vec![FirewallRef { firewall: 3 }],
            start_after_create: true,
        };
        let d = format!("{req:?} {req:#?}");
        assert!(
            !d.contains("mcsetup_SECRET") && !d.contains("install.sh"),
            "{d}"
        );
        assert!(d.contains("<redacted, ") && d.contains("modula-main") && d.contains("fsn1"));
        // The request itself still carries it.
        assert!(
            serde_json::to_string(&req)
                .unwrap()
                .contains("mcsetup_SECRET")
        );
    }

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
    fn type_changes_keep_arch_storage_and_disk() {
        let ty = |name: &str, disk: u32, arch: &str| -> ServerType {
            serde_json::from_value(json!({
                "name": name, "cores": 2, "memory": 4.0, "disk": disk,
                "architecture": arch, "storage_type": "local"
            }))
            .unwrap()
        };
        let server: Server = serde_json::from_value(json!({
            "id": 1, "name": "a", "status": "running",
            "server_type": { "name": "cx32", "cores": 4, "memory": 8.0, "disk": 80,
                             "architecture": "x86", "storage_type": "local" },
            "primary_disk_size": 80
        }))
        .unwrap();
        assert_eq!(server.change_type_blocker(&ty("cx42", 160, "x86")), None);
        assert_eq!(server.change_type_blocker(&ty("cx52", 80, "x86")), None);
        assert!(
            server
                .change_type_blocker(&ty("cx22", 40, "x86"))
                .unwrap()
                .contains("can't shrink")
        );
        assert!(
            server
                .change_type_blocker(&ty("cax31", 160, "arm"))
                .unwrap()
                .contains("architectures")
        );
        let mut network = ty("cpx31", 160, "x86");
        network.storage_type = "network".into();
        assert!(server.change_type_blocker(&network).is_some());
        // Upsized with the disk kept: back down is fine.
        let kept: Server = serde_json::from_value(json!({
            "id": 1, "name": "a", "status": "off",
            "server_type": { "name": "cx32", "cores": 4, "memory": 8.0, "disk": 80,
                             "architecture": "x86", "storage_type": "local" },
            "primary_disk_size": 40
        }))
        .unwrap();
        assert_eq!(kept.change_type_blocker(&ty("cx22", 40, "x86")), None);
    }

    #[test]
    fn deprecated_types_are_not_orderable() {
        let t: ServerType = serde_json::from_value(json!({
            "name": "cx22", "cores": 2, "memory": 4.0, "disk": 40,
            "architecture": "x86", "deprecation": null,
            "prices": [{ "location": "fsn1", "price_monthly": { "net": "3.79", "gross": "4.51" } }],
            "locations": [{ "name": "fsn1", "deprecation": null }, { "name": "nbg1", "deprecation": { "announced": "2025-01-01T00:00:00Z" } }, { "name": "hel1", "available": false }]
        }))
        .unwrap();
        assert!(t.orderable_in("fsn1"));
        assert!(!t.orderable_in("nbg1"), "retired in that location");
        assert!(!t.orderable_in("hel1"), "no price there");
        assert!(!t.orderable_in("ash"), "not listed there");
        let old: ServerType = serde_json::from_value(json!({
            "name": "cx11", "cores": 1, "memory": 2.0, "disk": 20, "deprecated": true,
            "prices": [{ "location": "fsn1", "price_monthly": { "net": "3", "gross": "3" } }]
        }))
        .unwrap();
        assert!(!old.orderable_in("fsn1"));
    }
}
