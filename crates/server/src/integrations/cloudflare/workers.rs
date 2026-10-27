//! Workers, uploaded directly (no Wrangler): the calls that publish a
//! function, or a static site with static assets, as a Worker.
//!
//! 1. `POST …/workers/scripts/{name}/assets-upload-session` with a manifest
//!    (`/path` → hash and size) answers with a JWT and the buckets of hashes
//!    Cloudflare doesn't have yet;
//! 2. each bucket goes to `POST …/workers/assets/upload?base64=true` as
//!    `multipart/form-data` (one part per hash, base64 contents), authorized
//!    by that JWT; the last one answers with the completion JWT (no buckets:
//!    the session JWT already is one);
//! 3. `PUT …/workers/scripts/{name}` uploads the Worker with its `assets`
//!    (completion JWT and serving config) and bindings (variables, secrets).
//!
//! The token needs Account · Workers Scripts · Edit.

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use super::client::{Client, Multipart, Payload, Result};

/// The Worker that fronts the assets. Requests that match a file never run
/// it (assets are served first); everything else is answered by the assets
/// binding too, so `not_found_handling` decides.
pub const WORKER_MODULE: &str =
    "export default { fetch(request, env) { return env.ASSETS.fetch(request); } };\n";
pub const WORKER_MODULE_NAME: &str = "worker.js";
pub const COMPATIBILITY_DATE: &str = "2025-01-01";
/// A function Worker's one module.
pub const FUNCTION_MODULE_NAME: &str = modula_proto::node::v1::FUNCTION_MODULE;

/// One file in an upload manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// `/index.html`.
    pub path: String,
    /// 32 lowercase hex characters.
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadSession {
    pub jwt: String,
    #[serde(default)]
    pub buckets: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadResult {
    #[serde(default)]
    jwt: Option<String>,
}

/// A file going up: its hash, MIME type and contents.
pub struct AssetFile<'a> {
    pub hash: &'a str,
    pub content_type: &'a str,
    pub data: &'a [u8],
}

/// A Worker binding to set with the script.
#[derive(Debug, Clone)]
pub enum Binding {
    Plain { name: String, text: String },
    Secret { name: String, text: String },
}

impl Binding {
    fn to_json(&self) -> Value {
        match self {
            Self::Plain { name, text } => {
                json!({ "type": "plain_text", "name": name, "text": text })
            }
            Self::Secret { name, text } => {
                json!({ "type": "secret_text", "name": name, "text": text })
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkersDomain {
    #[serde(default)]
    pub id: String,
    pub hostname: String,
    #[serde(default)]
    pub service: String,
}

impl Client {
    /// Step 1: which files Cloudflare still needs.
    pub async fn assets_upload_session(
        &self,
        account: &str,
        script: &str,
        manifest: &[ManifestEntry],
    ) -> Result<UploadSession> {
        let mut m = serde_json::Map::new();
        for e in manifest {
            m.insert(e.path.clone(), json!({ "hash": e.hash, "size": e.size }));
        }
        let path = format!("accounts/{account}/workers/scripts/{script}/assets-upload-session");
        self.call::<UploadSession>(Method::POST, &path, &[], Some(&json!({ "manifest": m })))
            .await?
            .0
            .ok_or_else(|| super::client::Error::Decode("upload session: empty result".into()))
    }

    /// Step 2: one bucket. Returns the completion JWT once every bucket is in.
    pub async fn upload_assets(
        &self,
        account: &str,
        session_jwt: &str,
        files: &[AssetFile<'_>],
    ) -> Result<Option<String>> {
        use base64::Engine as _;
        let mut form = Multipart::new();
        for f in files {
            let b64 = base64::engine::general_purpose::STANDARD.encode(f.data);
            form.part(f.hash, Some(f.hash), f.content_type, b64.as_bytes());
        }
        let form = form.finish();
        let (res, _) = self
            .request::<UploadResult>(
                Method::POST,
                &format!("accounts/{account}/workers/assets/upload"),
                &[("base64", "true".into())],
                Payload::Multipart(&form),
                Some(session_jwt),
            )
            .await?;
        Ok(res.and_then(|r| r.jwt).filter(|j| !j.is_empty()))
    }

    /// Step 3: the Worker, serving the uploaded assets.
    pub async fn put_assets_worker(
        &self,
        account: &str,
        script: &str,
        completion_jwt: &str,
        spa: bool,
        bindings: &[Binding],
    ) -> Result<()> {
        let mut all = vec![json!({ "type": "assets", "name": "ASSETS" })];
        all.extend(bindings.iter().map(Binding::to_json));
        let metadata = json!({
            "main_module": WORKER_MODULE_NAME,
            "compatibility_date": COMPATIBILITY_DATE,
            "assets": {
                "jwt": completion_jwt,
                "config": {
                    "html_handling": "auto-trailing-slash",
                    "not_found_handling": if spa { "single-page-application" } else { "404-page" },
                },
            },
            "bindings": all,
        });
        let mut form = Multipart::new();
        form.part(
            "metadata",
            None,
            "application/json",
            metadata.to_string().as_bytes(),
        );
        form.part(
            WORKER_MODULE_NAME,
            Some(WORKER_MODULE_NAME),
            "application/javascript+module",
            WORKER_MODULE.as_bytes(),
        );
        let form = form.finish();
        self.request::<Value>(
            Method::PUT,
            &format!("accounts/{account}/workers/scripts/{script}"),
            &[],
            Payload::Multipart(&form),
            None,
        )
        .await?;
        Ok(())
    }

    /// A function: `code` as the Worker's only module (module syntax), with
    /// `bindings` as its whole set (Modula is the source of truth).
    pub async fn put_module_worker(
        &self,
        account: &str,
        script: &str,
        code: &str,
        compatibility_date: &str,
        bindings: &[Binding],
    ) -> Result<()> {
        let metadata = json!({
            "main_module": FUNCTION_MODULE_NAME,
            "compatibility_date": compatibility_date,
            "bindings": bindings.iter().map(Binding::to_json).collect::<Vec<_>>(),
        });
        let mut form = Multipart::new();
        form.part(
            "metadata",
            None,
            "application/json",
            metadata.to_string().as_bytes(),
        );
        form.part(
            FUNCTION_MODULE_NAME,
            Some(FUNCTION_MODULE_NAME),
            "application/javascript+module",
            code.as_bytes(),
        );
        let form = form.finish();
        self.request::<Value>(
            Method::PUT,
            &format!("accounts/{account}/workers/scripts/{script}"),
            &[],
            Payload::Multipart(&form),
            None,
        )
        .await?;
        Ok(())
    }

    /// The account's `<subdomain>.workers.dev`, if it has claimed one.
    pub async fn workers_subdomain(&self, account: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Sub {
            #[serde(default)]
            subdomain: Option<String>,
        }
        match self
            .call::<Sub>(
                Method::GET,
                &format!("accounts/{account}/workers/subdomain"),
                &[],
                None,
            )
            .await
        {
            Ok((s, _)) => Ok(s.and_then(|s| s.subdomain).filter(|s| !s.is_empty())),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Serves the Worker on `<script>.<subdomain>.workers.dev`.
    pub async fn enable_workers_dev(&self, account: &str, script: &str) -> Result<()> {
        self.call::<Value>(
            Method::POST,
            &format!("accounts/{account}/workers/scripts/{script}/subdomain"),
            &[],
            Some(&json!({ "enabled": true, "previews_enabled": false })),
        )
        .await?;
        Ok(())
    }

    /// Routes `hostname` (in zone `zone_id`) to the Worker: Cloudflare
    /// creates the DNS record and the certificate.
    pub async fn attach_workers_domain(
        &self,
        account: &str,
        script: &str,
        hostname: &str,
        zone_id: &str,
    ) -> Result<WorkersDomain> {
        self.call::<WorkersDomain>(
            Method::PUT,
            &format!("accounts/{account}/workers/domains"),
            &[],
            Some(&json!({
                "hostname": hostname,
                "service": script,
                "zone_id": zone_id,
                "environment": "production",
            })),
        )
        .await?
        .0
        .ok_or_else(|| super::client::Error::Decode("workers domain: empty result".into()))
    }
}

/// The MIME type Cloudflare serves a file with, by extension.
pub fn content_type(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "application/javascript",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "xml" => "application/xml",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "md" => "text/markdown",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// A file's manifest hash: the first 32 hex characters of SHA-256 over its
/// base64 contents and extension (the scheme Cloudflare's own upload
/// example uses; Cloudflare only requires 32 hex characters, unique per
/// content).
pub fn asset_hash(path: &str, data: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let ext = path
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, e)| e)
        .unwrap_or_default();
    let mut h = Sha256::new();
    h.update(base64::engine::general_purpose::STANDARD.encode(data));
    h.update(ext);
    hex::encode(h.finalize())[..32].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_types() {
        let a = asset_hash("/index.html", b"<h1>hi</h1>");
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(
            a,
            asset_hash("/index.txt", b"<h1>hi</h1>"),
            "extension counts"
        );
        assert_ne!(a, asset_hash("/index.html", b"<h1>ho</h1>"));
        assert_eq!(content_type("/a/b.JS"), "application/javascript");
        assert_eq!(content_type("/LICENSE"), "application/octet-stream");
    }

    #[test]
    fn multipart_shape() {
        let mut m = Multipart::new();
        m.part("abc", Some("abc"), "text/html", b"PGgxPg==");
        let m = m.finish();
        assert!(
            m.content_type()
                .starts_with("multipart/form-data; boundary=----modula")
        );
        assert!(!m.is_empty());
    }
}
