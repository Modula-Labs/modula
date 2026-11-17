//! The contract every cloud provider implements, and the shapes it speaks,
//! shared by the control plane and the `modula` CLI. All money is in USD
//! cents: a provider billing in another currency converts when it produces
//! offers ([`eur_cents_to_usd`]).

pub mod locations;

use std::collections::BTreeMap;

use futures::FutureExt;
use futures::future::BoxFuture;
use serde::Serialize;

use crate::locations::Location;

/// What went wrong talking to the provider.
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

/// USD per EUR when nothing says otherwise (`MODULA_EUR_USD_RATE` on the
/// server; the CLI has no instance to ask).
pub const DEFAULT_EUR_USD_RATE: f64 = 1.08;

/// `eur_cents` in USD cents at `rate` (USD per EUR), to the nearest cent.
pub fn eur_cents_to_usd(eur_cents: i64, rate: f64) -> i64 {
    (eur_cents as f64 * rate).round() as i64
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
    /// Monthly price in USD cents, as billed (incl. VAT where the provider
    /// charges it); approximate for providers billing in another currency.
    pub monthly_cents: i64,
}

pub struct NewServer {
    pub name: String,
    pub region: String,
    pub server_type: String,
    pub user_data: String,
    pub labels: BTreeMap<String, String>,
    /// The provider's own key ids, as [`ServerProvider::ssh_keys`] lists them.
    pub ssh_key_ids: Vec<String>,
    /// Open port 22 in the server's firewall.
    pub ssh: bool,
    /// Root disk size, for providers where it is chosen separately from the
    /// server type; the others ignore it.
    pub disk_gb: Option<u32>,
}

/// An SSH key held at the provider.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSshKey {
    pub id: String,
    pub name: String,
    pub fingerprint: String,
    /// Its OpenSSH public key, when the provider lists it; for reusing a
    /// key already there.
    #[serde(skip)]
    pub public_key: Option<String>,
}

/// The public IPv4s allowed to a provider's WireGuard port.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeshSources {
    /// Servers peering with this workspace's (its own and shared ones).
    pub workspace: Vec<String>,
    /// Every server: what a main server's firewall admits.
    pub everyone: Vec<String>,
    /// Public IPv4s of this instance's own main servers. Only the firewalls
    /// on those are synced as main ones: an account may hold another
    /// instance's main server, labelled just like ours.
    pub mains: Vec<String>,
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

    /// The SSH keys servers can be created with.
    fn ssh_keys(&self) -> BoxFuture<'_, Result<Vec<ProviderSshKey>, ProviderError>>;

    /// `monthly_cents` (a server type's price at its default root disk)
    /// adjusted for a root disk of `disk_gb`, or why that disk isn't allowed.
    /// Providers where the disk comes with the type ignore it.
    fn price_with_disk(&self, monthly_cents: i64, _disk_gb: Option<u32>) -> Result<i64, String> {
        Ok(monthly_cents)
    }

    /// Bring the WireGuard rule of every Modula firewall this account holds
    /// up to date with `sources`, leaving its other rules as they are.
    /// Tries them all before reporting a failure.
    fn sync_mesh<'a>(
        &'a self,
        sources: &'a MeshSources,
    ) -> BoxFuture<'a, Result<(), ProviderError>>;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn euros_convert_to_the_nearest_cent() {
        assert_eq!(eur_cents_to_usd(452, 1.08), 488);
        assert_eq!(eur_cents_to_usd(821, 1.08), 887);
        assert_eq!(eur_cents_to_usd(0, 1.08), 0);
        assert_eq!(eur_cents_to_usd(1000, 1.0), 1000);
    }
}
