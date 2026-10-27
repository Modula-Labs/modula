//! S3-compatible storage: SigV4 signing (rusty-s3) and the few requests a
//! backup needs besides its part uploads. Shared by the control plane (for
//! storage it can reach) and modula-node (for a self-hosted bucket that only
//! the private network reaches). Callers bring their own SSRF-guarded HTTP
//! client.

use std::time::Duration;

use anyhow::{Context, bail};
use rusty_s3::actions::{
    AbortMultipartUpload, CompleteMultipartUpload, CreateMultipartUpload, DeleteObject, GetObject,
    PutObject, S3Action, UploadPart,
};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use url::Url;
use zeroize::Zeroizing;

use crate::stream::{s3_error_code, storage_error};

/// Presigned URLs handed to nodes must outlive the slowest dump or restore.
pub const NODE_URL_TTL: Duration = Duration::from_secs(13 * 3600);
/// Requests signed for immediate use.
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

#[derive(Clone)]
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

    pub async fn create_multipart(
        &self,
        http: &reqwest::Client,
        key: &str,
    ) -> anyhow::Result<String> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let url = CreateMultipartUpload::new(&bucket, Some(&creds), key).sign(OWN_URL_TTL);
        let body = send(http.post(url)).await?.text().await?;
        upload_id(&body)
    }

    pub async fn complete_multipart(
        &self,
        http: &reqwest::Client,
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
        let res = send(http.post(url).body(action.body())).await?;
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

    pub async fn abort_multipart(
        &self,
        http: &reqwest::Client,
        key: &str,
        upload_id: &str,
    ) -> anyhow::Result<()> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let url =
            AbortMultipartUpload::new(&bucket, Some(&creds), key, upload_id).sign(OWN_URL_TTL);
        send(http.delete(url)).await?;
        Ok(())
    }

    /// Delete an object; one that is already gone counts as deleted.
    pub async fn delete(&self, http: &reqwest::Client, key: &str) -> anyhow::Result<()> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let url = DeleteObject::new(&bucket, Some(&creds), key).sign(OWN_URL_TTL);
        let res = http
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

    /// Upload a small object (probes).
    pub async fn put(
        &self,
        http: &reqwest::Client,
        key: &str,
        body: Vec<u8>,
    ) -> anyhow::Result<()> {
        let (bucket, creds) = (self.bucket()?, self.credentials());
        let url = PutObject::new(&bucket, Some(&creds), key).sign(OWN_URL_TTL);
        send(http.put(url).body(body)).await?;
        Ok(())
    }

    /// Check the credentials can write and delete under the prefix.
    pub async fn probe(&self, http: &reqwest::Client, nonce: &str) -> anyhow::Result<()> {
        let key = self.key(&format!(".modula-probe-{nonce}"));
        self.put(http, &key, b"modula".to_vec())
            .await
            .context("cannot write to the bucket")?;
        self.delete(http, &key)
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
pub fn upload_id(xml: &str) -> anyhow::Result<String> {
    let created = CreateMultipartUpload::parse_response(xml)
        .map_err(|e| anyhow::anyhow!("unexpected CreateMultipartUpload response: {e}"))?;
    Ok(created.upload_id().to_owned())
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
    }
}
