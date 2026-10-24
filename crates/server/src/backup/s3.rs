//! S3-compatible storage targets: SigV4 signing (rusty-s3) and the few
//! requests the control plane makes itself. Nodes only ever get presigned
//! URLs for single objects, never the target's credentials.
//!
//! Endpoints are user input, so requests to them are SSRF-guarded: anything
//! but public addresses is refused, both as literal hosts and after DNS
//! resolution. Private and loopback ranges (MinIO on a LAN) need the
//! operator's opt-in; link-local (cloud metadata) is refused regardless.

use std::net::IpAddr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use modula_backup::net::{self, GuardedResolver};
use modula_backup::stream::{s3_error_code, storage_error};
use rusty_s3::actions::{
    AbortMultipartUpload, CompleteMultipartUpload, CreateMultipartUpload, DeleteObject, GetObject,
    PutObject, S3Action, UploadPart,
};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use url::{Host, Url};
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};

/// Presigned URLs handed to nodes must outlive the slowest dump or restore.
pub const NODE_URL_TTL: Duration = Duration::from_secs(13 * 3600);
/// The control plane's own requests are sent right away.
const OWN_URL_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    S3,
    R2,
    B2,
    Minio,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "s3" => Self::S3,
            "r2" => Self::R2,
            "b2" => Self::B2,
            "minio" => Self::Minio,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::R2 => "r2",
            Self::B2 => "b2",
            Self::Minio => "minio",
        }
    }
}

pub struct Target {
    pub provider: Provider,
    pub endpoint: Url,
    pub region: String,
    pub bucket: String,
    /// Empty, or ends with `/`.
    pub prefix: String,
    pub access_key_id: String,
    pub secret: Zeroizing<String>,
}

impl Target {
    fn bucket(&self) -> anyhow::Result<Bucket> {
        // AWS prefers virtual-hosted buckets, but dotted names break TLS there.
        let style = if self.provider == Provider::S3 && !self.bucket.contains('.') {
            UrlStyle::VirtualHost
        } else {
            UrlStyle::Path
        };
        Bucket::new(
            self.endpoint.clone(),
            style,
            self.bucket.clone(),
            self.region.clone(),
        )
        .map_err(|e| anyhow::anyhow!("invalid bucket: {e}"))
    }

    fn credentials(&self) -> Credentials {
        Credentials::new(self.access_key_id.clone(), self.secret.as_str())
    }

    /// The full object key for `name` under the target's prefix.
    pub fn key(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    pub fn presign_get(&self, key: &str) -> anyhow::Result<String> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        Ok(GetObject::new(&bucket, Some(&creds), key)
            .sign(NODE_URL_TTL)
            .into())
    }

    /// Presigned UploadPart URLs for parts 1..=count.
    pub fn presign_parts(
        &self,
        key: &str,
        upload_id: &str,
        count: u16,
    ) -> anyhow::Result<Vec<String>> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        Ok((1..=count)
            .map(|n| {
                UploadPart::new(&bucket, Some(&creds), key, n, upload_id)
                    .sign(NODE_URL_TTL)
                    .into()
            })
            .collect())
    }

    pub async fn create_multipart(&self, key: &str) -> anyhow::Result<String> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let url = CreateMultipartUpload::new(&bucket, Some(&creds), key).sign(OWN_URL_TTL);
        let body = send(http()?.post(url)).await?.text().await?;
        upload_id(&body)
    }

    pub async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> anyhow::Result<()> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let action = CompleteMultipartUpload::new(
            &bucket,
            Some(&creds),
            key,
            upload_id,
            etags.iter().map(String::as_str),
        );
        let url = action.sign(OWN_URL_TTL);
        let res = send(http()?.post(url).body(action.body())).await?;
        // S3 can report a failed completion inside a 200 response.
        let body = res.text().await?;
        if body.contains("<Error>") {
            bail!(
                "the storage could not assemble the upload: {}",
                s3_error_code(&body).unwrap_or("unknown error")
            );
        }
        Ok(())
    }

    pub async fn abort_multipart(&self, key: &str, upload_id: &str) -> anyhow::Result<()> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let url =
            AbortMultipartUpload::new(&bucket, Some(&creds), key, upload_id).sign(OWN_URL_TTL);
        send(http()?.delete(url)).await?;
        Ok(())
    }

    /// Delete an object; one that is already gone counts as deleted.
    pub async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let url = DeleteObject::new(&bucket, Some(&creds), key).sign(OWN_URL_TTL);
        let res = http()?
            .delete(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        let status = res.status();
        if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
            bail!("the storage answered {}", storage_error(res).await);
        }
        Ok(())
    }

    /// Upload a small object (the control plane's own uses: probes).
    pub async fn put(&self, key: &str, body: Vec<u8>) -> anyhow::Result<()> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let url = PutObject::new(&bucket, Some(&creds), key).sign(OWN_URL_TTL);
        send(http()?.put(url).body(body)).await?;
        Ok(())
    }

    /// Check the credentials can write and delete under the prefix.
    pub async fn probe(&self) -> anyhow::Result<()> {
        let key = self.key(&format!(".modula-probe-{}", modula_id::Id::new()));
        self.put(&key, b"modula".to_vec())
            .await
            .context("cannot write to the bucket")?;
        self.delete(&key)
            .await
            .context("cannot delete from the bucket")
    }
}

async fn send(req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
    let res = req.send().await.map_err(reqwest::Error::without_url)?;
    let status = res.status();
    if !status.is_success() {
        bail!("the storage answered {}", storage_error(res).await);
    }
    Ok(res)
}

/// The upload id from an `InitiateMultipartUploadResult`, which (like every
/// S3 response body) carries the S3 XML namespace.
fn upload_id(xml: &str) -> anyhow::Result<String> {
    let created = CreateMultipartUpload::parse_response(xml)
        .map_err(|e| anyhow::anyhow!("unexpected CreateMultipartUpload response: {e}"))?;
    Ok(created.upload_id().to_owned())
}

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

    fn target(provider: Provider, endpoint: &str, region: &str) -> Target {
        Target {
            provider,
            endpoint: endpoint.parse().unwrap(),
            region: region.into(),
            bucket: "backups".into(),
            prefix: "modula/".into(),
            access_key_id: "AKIDEXAMPLE".into(),
            secret: Zeroizing::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()),
        }
    }

    #[test]
    fn presigned_urls_follow_the_provider_style() {
        let minio = target(Provider::Minio, "http://10.0.0.5:9000", "us-east-1");
        let parts = minio
            .presign_parts(&minio.key("a/b.age"), "up1", 3)
            .unwrap();
        assert_eq!(parts.len(), 3);
        let first: Url = parts[0].parse().unwrap();
        assert_eq!(first.host_str(), Some("10.0.0.5"));
        assert_eq!(first.path(), "/backups/modula/a/b.age");
        let q: std::collections::HashMap<_, _> = first.query_pairs().into_owned().collect();
        assert_eq!(q["partNumber"], "1");
        assert_eq!(q["uploadId"], "up1");
        assert_eq!(q["X-Amz-Algorithm"], "AWS4-HMAC-SHA256");
        assert!(q["X-Amz-Credential"].starts_with("AKIDEXAMPLE/"));
        assert!(q["X-Amz-Credential"].ends_with("/us-east-1/s3/aws4_request"));
        assert_eq!(q["X-Amz-Signature"].len(), 64);
        assert!(parts[2].contains("partNumber=3"));
        assert!(!parts[0].contains("wJalr"), "the secret never appears");

        let s3 = target(
            Provider::S3,
            "https://s3.eu-central-1.amazonaws.com",
            "eu-central-1",
        );
        let get: Url = s3.presign_get("modula/x.age").unwrap().parse().unwrap();
        assert_eq!(
            get.host_str(),
            Some("backups.s3.eu-central-1.amazonaws.com")
        );
        assert_eq!(get.path(), "/modula/x.age");
    }

    #[test]
    fn upload_ids_are_read_from_the_documented_response() {
        // The example from the CreateMultipartUpload API reference; MinIO
        // answers the same way.
        let aws = r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
   <Bucket>example-bucket</Bucket>
   <Key>example-object</Key>
   <UploadId>VXBsb2FkIElEIGZvciA2aWWpbmcncyBteS1tb3ZpZS5tMnRzIHVwbG9hZA</UploadId>
</InitiateMultipartUploadResult>"#;
        assert_eq!(
            upload_id(aws).unwrap(),
            "VXBsb2FkIElEIGZvciA2aWWpbmcncyBteS1tb3ZpZS5tMnRzIHVwbG9hZA"
        );
        assert!(upload_id("<Error><Code>AccessDenied</Code></Error>").is_err());
        assert_eq!(
            s3_error_code("<Error><Code>AccessDenied</Code></Error>"),
            Some("AccessDenied")
        );
    }

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
