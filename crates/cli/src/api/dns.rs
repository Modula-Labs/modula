//! Name resolution for talking to an instance: the system resolver first,
//! then Cloudflare's and Google's resolvers directly when it finds nothing.
//!
//! A new server's domain is often looked up once before its DNS record
//! exists (by `server create`, a browser, `curl`), and the system resolver
//! (or the router's) then caches "no such name" for up to the zone's
//! negative TTL, half an hour on Cloudflare. Asking public resolvers fresh
//! sidesteps that cache, so the CLI keeps working while it expires.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use hickory_resolver::Resolver as Hickory;
use hickory_resolver::config::{CLOUDFLARE, GOOGLE, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{RData, RecordType};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

#[derive(Debug, Default, Clone, Copy)]
pub struct Fallback;

impl Resolve for Fallback {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            if let Ok(found) = tokio::net::lookup_host((host.as_str(), 0)).await {
                let found: Vec<SocketAddr> = found.collect();
                if !found.is_empty() {
                    return Ok(Box::new(found.into_iter()) as Addrs);
                }
            }
            let found = public(&host).await?;
            if found.is_empty() {
                return Err(format!("no address for {host}").into());
            }
            Ok(Box::new(found.into_iter().map(|ip| SocketAddr::new(ip, 0))) as Addrs)
        })
    }
}

/// `host`'s A and AAAA records from Cloudflare's and Google's resolvers.
async fn public(host: &str) -> Result<Vec<IpAddr>, Box<dyn std::error::Error + Send + Sync>> {
    let mut config = ResolverConfig::udp_and_tcp(&CLOUDFLARE);
    config.name_servers.extend(GOOGLE.udp_and_tcp());
    let mut builder = Hickory::builder_with_config(config, TokioRuntimeProvider::default());
    let opts = builder.options_mut();
    opts.timeout = Duration::from_secs(4);
    opts.attempts = 2;
    let resolver = builder.build()?;
    let fqdn = format!("{}.", host.trim_end_matches('.'));
    let mut out = Vec::new();
    for kind in [RecordType::A, RecordType::AAAA] {
        if let Ok(l) = resolver.lookup(fqdn.clone(), kind).await {
            out.extend(l.answers().iter().filter_map(|r| match &r.data {
                RData::A(a) => Some(IpAddr::V4(a.0)),
                RData::AAAA(a) => Some(IpAddr::V6(a.0)),
                _ => None,
            }));
        }
    }
    Ok(out)
}
