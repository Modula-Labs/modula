//! DNS providers: APIs that write records for the user (automatic CNAME/A
//! and TXT records for custom domains) and can answer ACME DNS-01 challenges
//! (wildcard certificates).
//!
//! The Cloudflare implementation lives in `integrations/cloudflare` and is
//! returned by [`super::provider_for`] once a workspace has a token. Until
//! then no provider exists: records are created by hand and wildcard domains
//! are rejected.

use futures::future::BoxFuture;

/// A DNS zone the provider manages (e.g. `example.com`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// The provider's id for the zone.
    pub id: String,
    pub name: String,
}

pub trait DnsProvider: Send + Sync {
    /// Short name for messages, e.g. "Cloudflare".
    fn name(&self) -> &'static str;
    /// The managed zone `hostname` lives in, if any.
    fn find_zone<'a>(&'a self, hostname: &'a str) -> BoxFuture<'a, anyhow::Result<Option<Zone>>>;
    /// Creates or updates the record `name` of type `kind` (`CNAME`, `A`,
    /// `TXT`) to `value`. `proxied` asks for the provider's proxy/CDN where
    /// it has one (ignored for TXT).
    fn ensure_record<'a>(
        &'a self,
        zone: &'a Zone,
        name: &'a str,
        kind: &'a str,
        value: &'a str,
        proxied: bool,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
    /// Removes the record `name` of type `kind`; absent records are not an error.
    fn delete_record<'a>(
        &'a self,
        zone: &'a Zone,
        name: &'a str,
        kind: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
    /// Whether the edge can solve DNS-01 through this provider (wildcards).
    fn supports_dns01(&self) -> bool;
}
