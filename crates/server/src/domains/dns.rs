//! DNS lookups for custom domain verification, behind a trait so tests can
//! answer without the network.

use std::net::Ipv4Addr;
use std::time::Duration;

use futures::future::BoxFuture;
use hickory_resolver::Resolver as Hickory;
use hickory_resolver::config::{CLOUDFLARE, GOOGLE, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::{RData, RecordType};

/// An empty answer means "no such record" (NXDOMAIN or no data); `Err` is a
/// lookup that failed (timeout, SERVFAIL) and says nothing about the record.
pub type Answer<T> = Result<Vec<T>, String>;

pub trait Resolver: Send + Sync + 'static {
    /// TXT strings at `name` (multi-string records joined).
    fn txt<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<String>>;
    /// CNAME targets at `name`, lowercase without the trailing dot.
    fn cname<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<String>>;
    fn ipv4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<Ipv4Addr>>;
}

/// Asks public resolvers (Cloudflare, then Google) rather than the host's, so
/// a split-horizon or stale local resolver can't verify a domain.
#[derive(Default)]
pub struct PublicDns;

impl PublicDns {
    async fn lookup(name: &str, rtype: RecordType) -> Answer<RData> {
        let mut config = ResolverConfig::udp_and_tcp(&CLOUDFLARE);
        config.name_servers.extend(GOOGLE.udp_and_tcp());
        let mut builder = Hickory::builder_with_config(config, TokioRuntimeProvider::default());
        let opts = builder.options_mut();
        opts.timeout = Duration::from_secs(5);
        opts.attempts = 2;
        // A fresh resolver per check, so "Check now" never sees a cached
        // NXDOMAIN from before the user added the record.
        let resolver = builder.build().map_err(|e| e.to_string())?;
        // Fully qualified: no search domains.
        let fqdn = format!("{}.", name.trim_end_matches('.'));
        match resolver.lookup(fqdn, rtype).await {
            Ok(l) => Ok(l.answers().iter().map(|r| r.data.clone()).collect()),
            Err(e) if e.is_nx_domain() || e.is_no_records_found() => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl Resolver for PublicDns {
    fn txt<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<String>> {
        Box::pin(async move {
            Ok(Self::lookup(name, RecordType::TXT)
                .await?
                .into_iter()
                .filter_map(|d| match d {
                    RData::TXT(t) => Some(
                        t.txt_data
                            .iter()
                            .map(|s| String::from_utf8_lossy(s).into_owned())
                            .collect(),
                    ),
                    _ => None,
                })
                .collect())
        })
    }

    fn cname<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<String>> {
        Box::pin(async move {
            Ok(Self::lookup(name, RecordType::CNAME)
                .await?
                .into_iter()
                .filter_map(|d| match d {
                    RData::CNAME(c) => {
                        Some(c.0.to_ascii().trim_end_matches('.').to_ascii_lowercase())
                    }
                    _ => None,
                })
                .collect())
        })
    }

    fn ipv4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<Ipv4Addr>> {
        Box::pin(async move {
            Ok(Self::lookup(name, RecordType::A)
                .await?
                .into_iter()
                .filter_map(|d| match d {
                    RData::A(a) => Some(a.0),
                    _ => None,
                })
                .collect())
        })
    }
}

/// Answers from a fixed table (tests).
#[derive(Default)]
pub struct StaticDns {
    pub records: std::sync::Mutex<std::collections::HashMap<(RecordKind, String), Vec<String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    Txt,
    Cname,
    A,
}

impl StaticDns {
    pub fn set(&self, kind: RecordKind, name: &str, values: &[&str]) {
        self.records.lock().expect("dns lock").insert(
            (kind, name.to_owned()),
            values.iter().map(|v| (*v).to_owned()).collect(),
        );
    }

    fn get(&self, kind: RecordKind, name: &str) -> Vec<String> {
        self.records
            .lock()
            .expect("dns lock")
            .get(&(kind, name.to_owned()))
            .cloned()
            .unwrap_or_default()
    }
}

impl Resolver for StaticDns {
    fn txt<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<String>> {
        Box::pin(async move { Ok(self.get(RecordKind::Txt, name)) })
    }

    fn cname<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<String>> {
        Box::pin(async move { Ok(self.get(RecordKind::Cname, name)) })
    }

    fn ipv4<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Answer<Ipv4Addr>> {
        Box::pin(async move {
            Ok(self
                .get(RecordKind::A, name)
                .iter()
                .filter_map(|v| v.parse().ok())
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits public DNS: `cargo test -p modula-server -- --ignored public_dns`.
    #[tokio::test]
    #[ignore]
    async fn public_dns() {
        let txt = PublicDns.txt("google.com").await.unwrap();
        assert!(txt.iter().any(|t| t.starts_with("v=spf1")), "{txt:?}");
        assert!(!PublicDns.ipv4("one.one.one.one").await.unwrap().is_empty());
        assert_eq!(
            PublicDns.cname("www.github.com").await.unwrap(),
            ["github.com"]
        );
        assert!(
            PublicDns
                .txt("_modula-challenge.nonexistent.example.com")
                .await
                .unwrap()
                .is_empty()
        );
    }
}
