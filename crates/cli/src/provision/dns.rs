//! Public DNS lookups: does the dashboard's domain point at the new server
//! yet? Asks Cloudflare's and Google's resolvers directly (a fresh resolver
//! each time, so a cached "no such name" from a minute ago doesn't stick),
//! since that's roughly what Let's Encrypt will see. Behind a trait so tests
//! answer without the network.

use std::net::Ipv4Addr;
use std::time::Duration;

use futures::future::BoxFuture;
use hickory_resolver::Resolver as Hickory;
use hickory_resolver::config::{CLOUDFLARE, GOOGLE, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{RData, RecordType};

pub trait Resolver: Send + Sync {
    /// The A records of `name`; empty when there are none (yet). `Err` is a
    /// failed lookup, which says nothing about the record.
    fn ipv4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Vec<Ipv4Addr>, String>>;
}

#[derive(Default)]
pub struct PublicDns;

impl Resolver for PublicDns {
    fn ipv4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Vec<Ipv4Addr>, String>> {
        Box::pin(async move {
            let mut config = ResolverConfig::udp_and_tcp(&CLOUDFLARE);
            config.name_servers.extend(GOOGLE.udp_and_tcp());
            let mut builder = Hickory::builder_with_config(config, TokioRuntimeProvider::default());
            let opts = builder.options_mut();
            opts.timeout = Duration::from_secs(5);
            opts.attempts = 2;
            let resolver = builder.build().map_err(|e| e.to_string())?;
            let fqdn = format!("{}.", name.trim_end_matches('.'));
            match resolver.lookup(fqdn, RecordType::A).await {
                Ok(l) => Ok(l
                    .answers()
                    .iter()
                    .filter_map(|r| match r.data {
                        RData::A(a) => Some(a.0),
                        _ => None,
                    })
                    .collect()),
                Err(e) if e.is_nx_domain() || e.is_no_records_found() => Ok(Vec::new()),
                Err(e) => Err(e.to_string()),
            }
        })
    }
}

/// Where `name` stands: pointing at `ip` only, nowhere yet, or elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pointing {
    Here,
    Nowhere,
    /// The addresses it has instead (or besides).
    Elsewhere(Vec<Ipv4Addr>),
}

pub fn pointing(answer: &[Ipv4Addr], ip: Ipv4Addr) -> Pointing {
    match answer {
        [] => Pointing::Nowhere,
        [only] if *only == ip => Pointing::Here,
        other => Pointing::Elsewhere(other.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointing_needs_exactly_this_ip() {
        let ip: Ipv4Addr = "203.0.113.10".parse().unwrap();
        let other: Ipv4Addr = "198.51.100.1".parse().unwrap();
        assert_eq!(pointing(&[ip], ip), Pointing::Here);
        assert_eq!(pointing(&[], ip), Pointing::Nowhere);
        assert_eq!(pointing(&[other], ip), Pointing::Elsewhere(vec![other]));
        // A leftover second record would send some of Let's Encrypt's checks elsewhere.
        assert_eq!(
            pointing(&[ip, other], ip),
            Pointing::Elsewhere(vec![ip, other])
        );
    }
}
