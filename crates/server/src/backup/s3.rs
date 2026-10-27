//! S3-compatible storage targets as the control plane uses them: the shared
//! signing and requests (`modula_backup::s3`), the SSRF-guarded client the
//! control plane sends them with, and validation of what users enter.
//!
//! Endpoints are user input, so requests to them are SSRF-guarded: anything
//! but public addresses is refused, both as literal hosts and after DNS
//! resolution. Private and loopback ranges (MinIO on a LAN) need the
//! operator's opt-in; link-local (cloud metadata) is refused regardless.

use std::net::IpAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use modula_backup::net::{self, GuardedResolver};
pub use modula_backup::s3::{NODE_URL_TTL, Provider, Target};
use url::{Host, Url};

use crate::error::{AppError, AppResult};

// ---------------------------------------------------------------- SSRF guard

static ALLOW_LOCAL: AtomicBool = AtomicBool::new(false);

/// Allow private and loopback endpoints (tests; operators set
/// `MODULA_BACKUP_ALLOW_PRIVATE_ENDPOINTS=1`, see [`net::ALLOW_PRIVATE_ENV`]).
pub fn allow_local_endpoints(allow: bool) {
    ALLOW_LOCAL.store(allow, Ordering::Relaxed);
}

fn local_allowed() -> bool {
    static FROM_ENV: OnceLock<bool> = OnceLock::new();
    ALLOW_LOCAL.load(Ordering::Relaxed) || *FROM_ENV.get_or_init(net::private_endpoints_from_env)
}

/// Whether the control plane may connect to `ip` on a user's behalf: public
/// addresses; private and loopback ones only with the opt-in; link-local
/// and cloud metadata never.
pub fn allowed_ip(ip: IpAddr) -> bool {
    net::backup_ip(ip, local_allowed())
}

pub fn http() -> anyhow::Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .user_agent(concat!("modula-deploy/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(std::sync::Arc::new(GuardedResolver::new(
            allowed_ip,
            "address backups may use",
        )))
        .no_proxy()
        .build()?;
    Ok(CLIENT.get_or_init(|| client))
}

// ---------------------------------------------------------------- input

fn bad(msg: &str) -> AppError {
    AppError::bad_request(msg)
}

/// The endpoint URL: `https://host[:port]` (http allowed, e.g. a LAN MinIO).
/// AWS S3 defaults to the regional endpoint.
pub fn endpoint(provider: Provider, raw: &str, region: &str) -> AppResult<Url> {
    let raw = raw.trim();
    let raw = match (raw.is_empty(), provider) {
        (false, _) => raw.to_owned(),
        (true, Provider::S3) => format!("https://s3.{region}.amazonaws.com"),
        (true, _) => return Err(bad("endpoint is required")),
    };
    let url: Url = raw.parse().map_err(|_| bad("endpoint must be a URL"))?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err(bad("endpoint must be an https:// URL"));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(bad(
            "endpoint must be just scheme, host and port (the bucket is separate)",
        ));
    }
    let ok = match url.host() {
        Some(Host::Ipv4(ip)) => allowed_ip(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => allowed_ip(IpAddr::V6(ip)),
        Some(Host::Domain(d)) => {
            local_allowed() || !(d == "localhost" || d.ends_with(".localhost"))
        }
        None => false,
    };
    if !ok {
        return Err(bad("endpoint must be a public address (private ones need \
             MODULA_BACKUP_ALLOW_PRIVATE_ENDPOINTS=1 on the server)"));
    }
    Ok(url)
}

pub fn region(provider: Provider, raw: &str) -> AppResult<String> {
    let r = raw.trim();
    let r = match (r.is_empty(), provider) {
        (false, _) => r,
        (true, Provider::R2) => "auto",
        (true, Provider::Minio) => "us-east-1",
        (true, _) => return Err(bad("region is required")),
    };
    if r.len() > 64
        || !r
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(bad("invalid region"));
    }
    Ok(r.to_owned())
}

pub fn bucket(raw: &str) -> AppResult<String> {
    let b = raw.trim();
    if !(3..=63).contains(&b.len())
        || !b
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
    {
        return Err(bad(
            "bucket must be 3–63 letters, digits, dots, hyphens or underscores",
        ));
    }
    Ok(b.to_owned())
}

/// `a/b` → `a/b/`; empty stays empty.
pub fn prefix(raw: &str) -> AppResult<String> {
    let p = raw.trim().trim_matches('/');
    if p.is_empty() {
        return Ok(String::new());
    }
    if p.len() > 200
        || p.contains("..")
        || p.contains("//")
        || !p
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'/'))
    {
        return Err(bad("invalid prefix"));
    }
    Ok(format!("{p}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_validated() {
        let ok = |raw: &str| endpoint(Provider::Minio, raw, "us-east-1").is_ok();
        assert!(ok("https://abc.r2.cloudflarestorage.com"));
        // Private addresses only with the operator's opt-in (off here).
        assert!(net::backup_ip("10.0.0.5".parse().unwrap(), true));
        for bad in [
            "http://10.0.0.5:9000",
            "http://127.0.0.1:9000",
            "http://169.254.169.254",
            "http://[::1]:9000",
            "http://[::ffff:127.0.0.1]",
            "http://localhost:9000",
            "http://0.0.0.0",
            "ftp://example.com",
            "https://user:pw@example.com",
            "https://example.com/bucket",
            "https://example.com/?x=1",
            "",
        ] {
            assert!(!ok(bad), "{bad}");
        }
        assert_eq!(
            endpoint(Provider::S3, "", "eu-west-1").unwrap().as_str(),
            "https://s3.eu-west-1.amazonaws.com/"
        );
        assert_eq!(region(Provider::R2, "").unwrap(), "auto");
        assert!(region(Provider::S3, "").is_err());
        assert_eq!(prefix("/team/db/").unwrap(), "team/db/");
        assert_eq!(prefix("").unwrap(), "");
        assert!(prefix("a/../b").is_err());
        assert!(bucket("my-bucket").is_ok());
        assert!(bucket("b/x").is_err());
    }
}
