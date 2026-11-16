//! Creating servers at a cloud provider, on explicit user action only.
//!
//! A provisioned server boots with a cloud-init script that installs Docker,
//! Caddy and modula-node and joins this control plane with a single-use,
//! 30-minute join token and the CA pin. A background job follows it from
//! `creating` → `booting` → `joining` → `ready` (or `failed`).
//!
//! Providers sit behind [`ServerProvider`]; [`PROVIDERS`] is the one place
//! that knows which exist.

pub mod api;
pub mod caps;
pub mod cloud_init;
pub mod mesh;
pub mod resize;
pub mod suggest;
pub mod track;

use futures::FutureExt;
use futures::future::BoxFuture;
use modula_id::Id;

use crate::error::{AppError, AppResult};
use crate::integrations::{aws, hetzner};
use crate::locations;
use crate::state::AppState;

/// How long a provisioned server has to join.
pub const JOIN_TOKEN_TTL: time::Duration = time::Duration::minutes(30);

pub use modula_provider::{
    ActionState, CreatedServer, LocationPrice, MeshSources, NewServer, Phase, Power, ProviderError,
    ProviderLocation, ProviderServer, ProviderSshKey, ServerProvider, ServerState, ServerTypeOffer,
};

impl From<ProviderError> for AppError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::Unauthorized | ProviderError::Forbidden => {
                AppError::bad_request(e.to_string())
            }
            ProviderError::Api { .. } => {
                AppError::Unavailable(format!("the cloud provider said: {e}"))
            }
            ProviderError::NotFound | ProviderError::Transport(_) => {
                AppError::Unavailable(e.to_string())
            }
        }
    }
}

type Connected<'a> = BoxFuture<'a, AppResult<Option<Box<dyn ServerProvider>>>>;
type Connect = for<'a> fn(&'a AppState, Id) -> Connected<'a>;

/// A cloud Modula can create servers at.
struct ProviderEntry {
    /// The integration kind, also `provisioned_servers.provider` and the
    /// catalog locations' provider.
    kind: &'static str,
    /// For messages.
    label: &'static str,
    /// Where to suggest a server when no location is given.
    default_location: &'static str,
    /// The workspace's connection, or None when not connected.
    connect: Connect,
}

/// Every server provider: adding one is an entry here and its own module.
const PROVIDERS: &[ProviderEntry] = &[
    ProviderEntry {
        kind: hetzner::KIND,
        label: "Hetzner",
        default_location: "hetzner:fsn1",
        connect: |state, workspace| {
            async move {
                Ok(hetzner::provider(state, workspace)
                    .await?
                    .map(|p| Box::new(p) as Box<dyn ServerProvider>))
            }
            .boxed()
        },
    },
    ProviderEntry {
        kind: aws::KIND,
        label: "AWS",
        default_location: "aws:us-east-1",
        connect: |state, workspace| {
            async move {
                Ok(aws::provider(state, workspace)
                    .await?
                    .map(|p| Box::new(p) as Box<dyn ServerProvider>))
            }
            .boxed()
        },
    },
];

fn entry(kind: &str) -> Option<&'static ProviderEntry> {
    PROVIDERS.iter().find(|p| p.kind == kind)
}

/// Integration kinds that are server providers.
pub fn provider_kinds() -> Vec<&'static str> {
    PROVIDERS.iter().map(|p| p.kind).collect()
}

/// `kind`'s name for messages.
pub fn label(kind: &str) -> &'static str {
    entry(kind).map_or("the cloud provider", |p| p.label)
}

/// The providers' names for messages: `Hetzner`, `Hetzner or AWS`.
pub fn labels() -> String {
    let all: Vec<&str> = PROVIDERS.iter().map(|p| p.label).collect();
    all.join(" or ")
}

/// The workspace's provider of `kind`, or None when not connected.
pub async fn provider_for(
    state: &AppState,
    workspace: Id,
    kind: &str,
) -> AppResult<Option<Box<dyn ServerProvider>>> {
    match entry(kind) {
        Some(p) => (p.connect)(state, workspace).await,
        None => Ok(None),
    }
}

/// The workspace's provider of `kind`, or a conflict saying to connect it.
pub async fn connected_provider(
    state: &AppState,
    workspace: Id,
    kind: &str,
) -> AppResult<Box<dyn ServerProvider>> {
    provider_for(state, workspace, kind).await?.ok_or_else(|| {
        AppError::Conflict(format!(
            "connect {} in workspace settings first",
            label(kind)
        ))
    })
}

/// The provider that sells catalog location `id` (`hetzner:fsn1`), when
/// Modula can create servers there.
pub fn kind_of_location(id: &str) -> Option<&'static str> {
    let provider = locations::get(id)?.provider;
    entry(provider).map(|p| p.kind)
}

/// The kinds `workspace` has connected, in [`PROVIDERS`] order.
pub async fn connected_kinds<'e>(
    db: impl sqlx::PgExecutor<'e>,
    workspace: Id,
) -> AppResult<Vec<&'static str>> {
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM integrations WHERE workspace_id = $1 AND kind = ANY($2)",
    )
    .bind(workspace)
    .bind(provider_kinds())
    .fetch_all(db)
    .await?;
    Ok(PROVIDERS
        .iter()
        .map(|p| p.kind)
        .filter(|k| kinds.iter().any(|c| c == k))
        .collect())
}

/// Where to suggest a server when the request names no location: the first
/// connected provider's default.
pub fn default_location(connected: &[&str]) -> Option<&'static str> {
    let first = connected.first()?;
    entry(first).map(|p| p.default_location)
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
    fn the_location_decides_the_provider() {
        assert_eq!(kind_of_location("hetzner:fsn1"), Some(hetzner::KIND));
        // In the catalog, but no provider creates servers there.
        assert_eq!(kind_of_location("gcp:us-central1"), None);
        assert_eq!(kind_of_location("nowhere:else"), None);
        assert_eq!(default_location(&[hetzner::KIND]), Some("hetzner:fsn1"));
        assert_eq!(kind_of_location("aws:eu-west-1"), Some(aws::KIND));
        assert_eq!(default_location(&[aws::KIND]), Some("aws:us-east-1"));
        assert_eq!(default_location(&[]), None);
    }

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
