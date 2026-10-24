//! Cloudflare as a [`DnsProvider`] for custom domains.

use futures::future::BoxFuture;

use super::Client;
use crate::domains::provider::{DnsProvider, Zone};

pub struct CloudflareDns {
    client: Client,
}

impl CloudflareDns {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl DnsProvider for CloudflareDns {
    fn name(&self) -> &'static str {
        "Cloudflare"
    }

    fn find_zone<'a>(&'a self, hostname: &'a str) -> BoxFuture<'a, anyhow::Result<Option<Zone>>> {
        Box::pin(async move {
            Ok(self
                .client
                .find_zone(hostname)
                .await?
                // A zone still pending nameserver change can't serve records yet.
                .filter(|z| z.status == "active")
                .map(|z| Zone {
                    id: z.id,
                    name: z.name,
                }))
        })
    }

    fn ensure_record<'a>(
        &'a self,
        zone: &'a Zone,
        name: &'a str,
        kind: &'a str,
        value: &'a str,
        proxied: bool,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.client
                .ensure_record(&zone.id, name, kind, value, proxied)
                .await?;
            Ok(())
        })
    }

    fn set_routing<'a>(
        &'a self,
        zone: &'a Zone,
        name: &'a str,
        want: &'a [(&'a str, &'a str)],
        proxied: bool,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.client
                .set_routing(&zone.id, name, want, proxied)
                .await?;
            Ok(())
        })
    }

    fn delete_record<'a>(
        &'a self,
        zone: &'a Zone,
        name: &'a str,
        kind: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.client.delete_records(&zone.id, name, kind).await?;
            Ok(())
        })
    }

    /// TODO: DNS-01 needs the token on every edge node (Caddy's
    /// `caddy-dns/cloudflare` issuer); nothing delivers it there yet, so
    /// wildcard domains stay unavailable.
    fn supports_dns01(&self) -> bool {
        false
    }
}
