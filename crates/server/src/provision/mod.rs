//! Creating servers at a cloud provider, on explicit user action only.
//!
//! A provisioned server boots with a cloud-init script that installs Docker,
//! Caddy and modula-node and joins this control plane with a single-use,
//! 30-minute join token and the CA pin. A background job follows it from
//! `creating` → `booting` → `joining` → `ready` (or `failed`).
//!
//! Providers sit behind [`ServerProvider`] so other clouds (or plain SSH)
//! can follow Hetzner.

pub mod api;
pub mod cloud_init;
pub mod resize;
pub mod suggest;
pub mod track;

use std::collections::BTreeMap;

use futures::FutureExt;
use futures::future::BoxFuture;
use serde::Serialize;

use crate::error::AppError;
use crate::locations::Location;

/// How long a provisioned server has to join.
pub const JOIN_TOKEN_TTL: time::Duration = time::Duration::minutes(30);

/// Shared with the CLI's Hetzner client (`modula-hetzner`).
pub use modula_hetzner::ProviderError;

impl From<ProviderError> for AppError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::Unauthorized | ProviderError::Forbidden => {
                AppError::bad_request(e.to_string())
            }
            ProviderError::Api { .. } => AppError::Unavailable(format!("Hetzner: {e}")),
            ProviderError::NotFound | ProviderError::Transport(_) => {
                AppError::Unavailable(e.to_string())
            }
        }
    }
}

/// A region the provider can create servers in, mapped to our catalog.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderLocation {
    /// The provider's own name, e.g. `fsn1`.
    pub region: String,
    pub location: &'static Location,
}

/// A server size with its monthly price per location.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTypeOffer {
    pub name: String,
    pub description: String,
    pub cores: u32,
    pub memory_gb: f64,
    pub disk_gb: u32,
    /// `x86` or `arm`.
    pub arch: String,
    /// `shared` or `dedicated`.
    pub cpu_type: String,
    /// `local` or `network` (empty when the provider doesn't say).
    #[serde(default)]
    pub storage_type: String,
    /// Only locations where it can be ordered now: a type missing here is
    /// sold out, retired or never offered in that location.
    pub prices: Vec<LocationPrice>,
}

impl ServerTypeOffer {
    /// The monthly price in `location_id`, when the type can be ordered there.
    pub fn price_in(&self, location_id: &str) -> Option<i64> {
        self.prices
            .iter()
            .find(|p| p.location == location_id)
            .map(|p| p.monthly_cents)
    }

    pub fn orderable_in(&self, location_id: &str) -> bool {
        self.price_in(location_id).is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationPrice {
    /// Catalog id, e.g. `hetzner:fsn1`.
    pub location: &'static str,
    /// Gross (incl. VAT) monthly price in euro cents.
    pub monthly_cents: i64,
}

pub struct NewServer {
    pub name: String,
    pub region: String,
    pub server_type: String,
    pub user_data: String,
    pub labels: BTreeMap<String, String>,
    pub ssh_key_ids: Vec<i64>,
    /// Open port 22 in the server's firewall.
    pub ssh: bool,
}

#[derive(Debug, Clone)]
pub struct CreatedServer {
    pub id: String,
    pub firewall_id: Option<String>,
    pub action_id: Option<String>,
    pub ipv4: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Booting,
    Running,
    /// Shutting down: not off yet.
    Stopping,
    Off,
    Gone,
}

#[derive(Debug, Clone)]
pub struct ServerState {
    pub phase: Phase,
    pub ipv4: Option<String>,
    /// Set when the provider's create action failed.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    On,
    /// A clean shutdown through the operating system (ACPI); the OS may ignore it.
    Off,
    /// Cut the power: for when [`Power::Off`] did nothing.
    ForceOff,
    Reboot,
}

/// A server as the provider describes it, whether or not Modula created it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderServer {
    pub id: String,
    pub name: String,
    pub ipv4: Option<String>,
    pub phase: Phase,
    /// Catalog location, when the provider's region maps to one.
    pub location: Option<&'static Location>,
    /// Its current type, which may no longer be sold.
    pub server_type: String,
    pub cores: u32,
    pub memory_gb: f64,
    /// The disk it has now (can be bigger than its type's after a resize).
    pub disk_gb: u32,
    pub arch: String,
    pub storage_type: String,
    /// `main` for a control-plane server created by `modula server create`.
    pub role: Option<String>,
}

/// A provider action (resize, …) being followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionState {
    Running,
    Done,
    Failed(String),
}

/// A cloud that can create servers. Every call uses the workspace's own
/// credentials; implementations never log them.
pub trait ServerProvider: Send + Sync {
    fn kind(&self) -> &'static str;
    fn locations(&self) -> BoxFuture<'_, Result<Vec<ProviderLocation>, ProviderError>>;
    /// Orderable server types only (deprecated ones are hidden).
    fn server_types(&self) -> BoxFuture<'_, Result<Vec<ServerTypeOffer>, ProviderError>>;
    fn create<'a>(
        &'a self,
        req: &'a NewServer,
    ) -> BoxFuture<'a, Result<CreatedServer, ProviderError>>;
    /// Find a server we created by its `modula-id` label (recovers from a
    /// crash between creating it and recording its id).
    fn find<'a>(
        &'a self,
        modula_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<CreatedServer>, ProviderError>>;
    /// Succeeds when the server is already gone.
    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), ProviderError>>;
    fn power<'a>(&'a self, id: &'a str, op: Power) -> BoxFuture<'a, Result<(), ProviderError>>;
    fn status<'a>(
        &'a self,
        id: &'a str,
        action_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ServerState, ProviderError>>;

    /// The types that can be ordered in `location_id` (a catalog id) now.
    fn server_types_at<'a>(
        &'a self,
        location_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ServerTypeOffer>, ProviderError>> {
        async move {
            let mut types = self.server_types().await?;
            types.retain(|t| t.orderable_in(location_id));
            Ok(types)
        }
        .boxed()
    }

    /// Describe one server; None when it no longer exists.
    fn describe<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProviderServer>, ProviderError>>;

    /// A Modula server (created here or by `modula server create`) with this
    /// public IPv4, for linking a node to the provider's server.
    fn find_by_address<'a>(
        &'a self,
        ipv4: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProviderServer>, ProviderError>>;

    /// Why `server` can't change to `to` in place, or None when it can
    /// (provider rules such as architecture or disk size).
    fn resize_blocker(&self, server: &ProviderServer, to: &ServerTypeOffer) -> Option<String>;

    /// Change a powered-off server's type, keeping its disk so it can be
    /// downsized again. Returns the action to follow with [`Self::action`].
    fn resize<'a>(
        &'a self,
        id: &'a str,
        server_type: &'a str,
    ) -> BoxFuture<'a, Result<String, ProviderError>>;

    fn action<'a>(
        &'a self,
        action_id: &'a str,
    ) -> BoxFuture<'a, Result<ActionState, ProviderError>>;

    /// Let every server reach `id`'s node endpoint (`port`/tcp, plus what a
    /// main server serves): it becomes the main server (migrate/promote.rs).
    /// Idempotent.
    fn open_node_port<'a>(
        &'a self,
        id: &'a str,
        name: &'a str,
        port: u16,
    ) -> BoxFuture<'a, Result<(), ProviderError>>;
}

/// Server names become hostnames and node names: lowercase DNS labels.
pub fn valid_server_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=63).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names_are_dns_labels() {
        assert!(valid_server_name("worker-1"));
        assert!(valid_server_name("a"));
        assert!(!valid_server_name(""));
        assert!(!valid_server_name("-a"));
        assert!(!valid_server_name("a-"));
        assert!(!valid_server_name("Worker"));
        assert!(!valid_server_name("a b"));
        assert!(!valid_server_name("a';rm -rf /;'"));
        assert!(!valid_server_name(&"a".repeat(64)));
    }
}
