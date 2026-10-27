//! A small client for the Cloudflare v4 REST API: just the calls Modula uses.
//!
//! Every response is wrapped in the same envelope
//! (`{success, errors, result, result_info}`); list endpoints are paginated
//! with `page`/`per_page`. A 429 is retried once after `Retry-After`
//! (Cloudflare allows 1200 requests per 5 minutes per user); nothing else is
//! retried. The token is only ever sent in the `Authorization` header and is
//! never logged.

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::{Method, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

const USER_AGENT: &str = concat!("modula-server/", env!("CARGO_PKG_VERSION"));
const PER_PAGE: u32 = 50;
/// Safety cap on paginated listings (2500 zones or records).
const MAX_PAGES: u32 = 50;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

/// The production API (`Config::cloudflare_api` points tests at a mock).
pub const API_URL: &str = "https://api.cloudflare.com/client/v4";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Cloudflare answered with an error envelope (or a bare status).
    #[error("Cloudflare: {message} (HTTP {status})")]
    Api { status: u16, message: String },
    #[error("cannot reach Cloudflare: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected response from Cloudflare: {0}")]
    Decode(String),
}

impl Error {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The token is valid but lacks a permission (or the resource is outside
    /// its scope).
    pub fn is_forbidden(&self) -> bool {
        matches!(self.status(), Some(401 | 403))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Deserialize)]
pub struct TokenStatus {
    pub id: String,
    /// `active`, `disabled` or `expired`.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
    /// `active`, `pending`, `initializing`, `moved`, …
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub proxied: Option<bool>,
    #[serde(default)]
    pub comment: Option<String>,
}

/// Marks records Modula wrote; only those are ever deleted.
pub const MANAGED_COMMENT: &str = "Managed by Modula Deploy";

/// What [`Client::ensure_record`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Unchanged,
    Created,
    Updated,
}

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<ApiMessage>,
    result: Option<T>,
    #[serde(default)]
    result_info: Option<ResultInfo>,
}

#[derive(Deserialize)]
struct ApiMessage {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize, Default, Clone, Copy)]
pub(super) struct ResultInfo {
    #[serde(default)]
    total_pages: u32,
}

/// One HTTP client for the process (connection reuse across workspaces).
fn http() -> &'static reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(|| {
        // ring, like the rest of the server; installing twice is a no-op.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("HTTP client")
    })
}

/// A Cloudflare API client bound to one token.
#[derive(Clone)]
pub struct Client {
    base: Url,
    token: SecretString,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base.as_str())
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(base: Url, token: SecretString) -> Self {
        Self { base, token }
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Result<Url> {
        let raw = format!("{}/{}", self.base.as_str().trim_end_matches('/'), path);
        let mut url = Url::parse(&raw).map_err(|e| Error::Decode(e.to_string()))?;
        if !query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }

    /// One request; returns the envelope's `result` and `result_info`.
    pub(super) async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(Option<T>, ResultInfo)> {
        let payload = match body {
            Some(b) => Payload::Json(b),
            None => Payload::Empty,
        };
        self.request(method, path, query, payload, None).await
    }

    /// [`call`](Self::call) with any body, authorized by `bearer` instead of
    /// the API token when given (e.g. an assets upload session's JWT).
    pub(super) async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Payload<'_>,
        bearer: Option<&str>,
    ) -> Result<(Option<T>, ResultInfo)> {
        let url = self.url(path, query)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = http()
                .request(method.clone(), url.clone())
                .bearer_auth(bearer.unwrap_or(self.token.expose_secret()));
            match &body {
                Payload::Empty => {}
                Payload::Json(b) => req = req.json(b),
                Payload::Multipart(m) => {
                    req = req
                        .header(reqwest::header::CONTENT_TYPE, m.content_type())
                        .body(m.body.clone());
                }
            }
            let res = req.send().await?;
            let status = res.status();
            if status == StatusCode::TOO_MANY_REQUESTS && attempt == 1 {
                let wait = res
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map_or(Duration::from_secs(1), Duration::from_secs)
                    .min(MAX_RETRY_AFTER);
                tokio::time::sleep(wait).await;
                continue;
            }
            let bytes = res.bytes().await?;
            let env: Option<Envelope<T>> = serde_json::from_slice(&bytes).ok();
            return match env {
                Some(env) if status.is_success() && env.success => {
                    Ok((env.result, env.result_info.unwrap_or_default()))
                }
                Some(env) => Err(Error::Api {
                    status: status.as_u16(),
                    message: describe(&env.errors, status),
                }),
                None if status.is_success() => Err(Error::Decode(format!(
                    "{method} {path}: not a Cloudflare API response"
                ))),
                None => Err(Error::Api {
                    status: status.as_u16(),
                    message: status.canonical_reason().unwrap_or("error").to_owned(),
                }),
            };
        }
    }

    pub(super) async fn get_one<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.call(Method::GET, path, &[], None)
            .await?
            .0
            .ok_or_else(|| Error::Decode(format!("GET {path}: empty result")))
    }

    /// Every page of a list endpoint.
    pub(super) async fn list<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Vec<T>> {
        let mut out = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut q = query.to_vec();
            q.push(("page", page.to_string()));
            q.push(("per_page", PER_PAGE.to_string()));
            let (items, info): (Option<Vec<T>>, _) = self.call(Method::GET, path, &q, None).await?;
            let items = items.unwrap_or_default();
            let n = items.len();
            out.extend(items);
            let last = if info.total_pages > 0 {
                page >= info.total_pages
            } else {
                n < PER_PAGE as usize
            };
            if last || n == 0 {
                break;
            }
        }
        Ok(out)
    }

    /// `GET /user/tokens/verify`: whether the token exists and is active.
    pub async fn verify(&self) -> Result<TokenStatus> {
        self.get_one("user/tokens/verify").await
    }

    /// Accounts the token can see. Zone-scoped tokens may see none.
    pub async fn accounts(&self) -> Result<Vec<Account>> {
        self.list("accounts", &[]).await
    }

    pub async fn zones(&self) -> Result<Vec<Zone>> {
        self.list("zones", &[]).await
    }

    /// The zone named exactly `name`, if the token can see it.
    pub async fn zone_named(&self, name: &str) -> Result<Option<Zone>> {
        let (zones, _): (Option<Vec<Zone>>, _) = self
            .call(Method::GET, "zones", &[("name", name.to_owned())], None)
            .await?;
        Ok(zones
            .unwrap_or_default()
            .into_iter()
            .find(|z| z.name.eq_ignore_ascii_case(name)))
    }

    /// The deepest zone `hostname` lives in (`a.b.example.com` → `example.com`),
    /// trying each parent name in turn so large accounts need no full listing.
    pub async fn find_zone(&self, hostname: &str) -> Result<Option<Zone>> {
        let host = hostname
            .trim_start_matches("*.")
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let labels: Vec<&str> = host.split('.').collect();
        // Zones have at least two labels.
        for i in 0..labels.len().saturating_sub(1) {
            let candidate = labels[i..].join(".");
            if let Some(z) = self.zone_named(&candidate).await? {
                return Ok(Some(z));
            }
        }
        Ok(None)
    }

    /// The zone's SSL/TLS encryption mode: `off`, `flexible`, `full` or `strict`.
    pub async fn ssl_mode(&self, zone_id: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Setting {
            value: String,
        }
        let s: Setting = self
            .get_one(&format!("zones/{zone_id}/settings/ssl"))
            .await?;
        Ok(s.value)
    }

    pub async fn dns_records(
        &self,
        zone_id: &str,
        name: &str,
        kind: &str,
    ) -> Result<Vec<DnsRecord>> {
        self.list(
            &format!("zones/{zone_id}/dns_records"),
            &[("type", kind.to_owned()), ("name", name.to_owned())],
        )
        .await
    }

    /// Makes `name` have a `kind` record with `value`. Idempotent: an equal
    /// record is left alone. A CNAME (only one may exist per name) is updated
    /// in place; for A/AAAA/TXT the value is added next to any existing ones,
    /// so several servers can share a name. `proxied` applies to A/AAAA/CNAME.
    pub async fn ensure_record(
        &self,
        zone_id: &str,
        name: &str,
        kind: &str,
        value: &str,
        proxied: bool,
    ) -> Result<Change> {
        let kind = kind.to_ascii_uppercase();
        let proxiable = matches!(kind.as_str(), "A" | "AAAA" | "CNAME");
        let existing = self.dns_records(zone_id, name, &kind).await?;
        let path = format!("zones/{zone_id}/dns_records");
        let same = existing
            .iter()
            .find(|r| same_content(&kind, &r.content, value));
        let to_update = match same {
            Some(r) if !proxiable || r.proxied.unwrap_or(false) == proxied => {
                return Ok(Change::Unchanged);
            }
            Some(r) => Some(r),
            None if kind == "CNAME" => existing.first(),
            None => None,
        };
        let mut body = json!({
            "type": kind, "name": name, "content": value, "ttl": 1, "comment": MANAGED_COMMENT,
        });
        if proxiable {
            body["proxied"] = json!(proxied);
        }
        match to_update {
            Some(r) => {
                self.call::<Value>(Method::PATCH, &format!("{path}/{}", r.id), &[], Some(&body))
                    .await?;
                Ok(Change::Updated)
            }
            None => {
                self.call::<Value>(Method::POST, &path, &[], Some(&body))
                    .await?;
                Ok(Change::Created)
            }
        }
    }

    /// Makes the routing records (A/AAAA/CNAME) at `name` exactly `want`
    /// (`(kind, value)` pairs): Modula's own records that aren't wanted go,
    /// the wanted ones are ensured. A CNAME can't sit next to other records,
    /// so a conflicting kind is removed before the new records are written;
    /// stale values of the same kind go afterwards (no gap while switching
    /// servers). The user's own records are never touched.
    pub async fn set_routing(
        &self,
        zone_id: &str,
        name: &str,
        want: &[(&str, &str)],
        proxied: bool,
    ) -> Result<()> {
        // Nothing to point at (no server address known yet): leave the
        // records alone rather than delete them.
        if want.is_empty() {
            return Ok(());
        }
        let mut managed = Vec::new();
        for kind in ["A", "AAAA", "CNAME"] {
            managed.extend(
                self.dns_records(zone_id, name, kind)
                    .await?
                    .into_iter()
                    .filter(|r| r.comment.as_deref() == Some(MANAGED_COMMENT)),
            );
        }
        let wanted = |r: &DnsRecord| {
            want.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case(&r.kind) && same_content(&r.kind, &r.content, v)
            })
        };
        let wants_cname = want.iter().any(|(k, _)| k.eq_ignore_ascii_case("CNAME"));
        let (conflicting, stale): (Vec<&DnsRecord>, Vec<&DnsRecord>) = managed
            .iter()
            .filter(|r| !wanted(r))
            .partition(|r| wants_cname || r.kind == "CNAME");
        for r in conflicting {
            self.delete_record_id(zone_id, &r.id).await?;
        }
        for (kind, value) in want {
            self.ensure_record(zone_id, name, kind, value, proxied)
                .await?;
        }
        for r in stale {
            self.delete_record_id(zone_id, &r.id).await?;
        }
        Ok(())
    }

    async fn delete_record_id(&self, zone_id: &str, id: &str) -> Result<()> {
        let path = format!("zones/{zone_id}/dns_records/{id}");
        match self.call::<Value>(Method::DELETE, &path, &[], None).await {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Removes the `kind` records named `name` that Modula wrote (see
    /// [`MANAGED_COMMENT`]); the user's own records are left alone. Returns
    /// how many went.
    pub async fn delete_records(&self, zone_id: &str, name: &str, kind: &str) -> Result<usize> {
        let kind = kind.to_ascii_uppercase();
        let mut n = 0;
        let records = self.dns_records(zone_id, name, &kind).await?;
        for r in records
            .into_iter()
            .filter(|r| r.comment.as_deref() == Some(MANAGED_COMMENT))
        {
            let path = format!("zones/{zone_id}/dns_records/{}", r.id);
            match self.call::<Value>(Method::DELETE, &path, &[], None).await {
                Ok(_) => n += 1,
                Err(e) if e.status() == Some(404) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(n)
    }

    /// Whether the token may use R2 in `account_id` (lists at most one bucket).
    pub async fn r2_access(&self, account_id: &str) -> Result<bool> {
        let path = format!("accounts/{account_id}/r2/buckets");
        match self
            .call::<Value>(Method::GET, &path, &[("per_page", "1".into())], None)
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if e.is_forbidden() => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Creates an R2 bucket; one that already exists in the account is fine.
    pub async fn create_r2_bucket(&self, account_id: &str, name: &str) -> Result<Change> {
        let path = format!("accounts/{account_id}/r2/buckets");
        match self
            .call::<Value>(Method::POST, &path, &[], Some(&json!({ "name": name })))
            .await
        {
            Ok(_) => Ok(Change::Created),
            Err(e) if e.status() == Some(409) => Ok(Change::Unchanged),
            Err(e) => Err(e),
        }
    }
}

/// A request body.
pub(super) enum Payload<'a> {
    Empty,
    Json(&'a Value),
    Multipart(&'a Multipart),
}

/// A `multipart/form-data` body, built in memory.
pub struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Default for Multipart {
    fn default() -> Self {
        Self::new()
    }
}

impl Multipart {
    pub fn new() -> Self {
        Self {
            boundary: format!("----modula{}{}", modula_id::Id::new(), modula_id::Id::new()),
            body: Vec::new(),
        }
    }

    /// Adds a part. `filename` makes it a file part.
    pub fn part(&mut self, name: &str, filename: Option<&str>, content_type: &str, data: &[u8]) {
        let disposition = match filename {
            Some(f) => format!("form-data; name=\"{name}\"; filename=\"{f}\""),
            None => format!("form-data; name=\"{name}\""),
        };
        self.body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: {disposition}\r\nContent-Type: {content_type}\r\n\r\n",
                self.boundary
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(data);
        self.body.extend_from_slice(b"\r\n");
    }

    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    /// The finished body (closing boundary included).
    pub fn finish(mut self) -> Self {
        self.body
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        self
    }

    pub fn len(&self) -> usize {
        self.body.len()
    }

    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }
}

fn describe(errors: &[ApiMessage], status: StatusCode) -> String {
    let msgs: Vec<String> = errors
        .iter()
        .map(|e| {
            if e.code != 0 {
                format!("{} [{}]", e.message, e.code)
            } else {
                e.message.clone()
            }
        })
        .collect();
    if msgs.is_empty() {
        status.canonical_reason().unwrap_or("error").to_owned()
    } else {
        msgs.join("; ")
    }
}

/// Record contents compare loosely: hostnames ignore case and a trailing dot,
/// and Cloudflare may return TXT content wrapped in quotes.
fn same_content(kind: &str, have: &str, want: &str) -> bool {
    match kind {
        "CNAME" => have
            .trim_end_matches('.')
            .eq_ignore_ascii_case(want.trim_end_matches('.')),
        "TXT" => have.trim_matches('"') == want.trim_matches('"'),
        _ => have == want,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_comparison() {
        assert!(same_content("CNAME", "App.Example.com.", "app.example.com"));
        assert!(same_content(
            "TXT",
            "\"modula-verify=ab\"",
            "modula-verify=ab"
        ));
        assert!(!same_content("A", "203.0.113.7", "203.0.113.8"));
    }

    #[test]
    fn urls_keep_the_base_path() {
        let c = Client::new(
            "http://127.0.0.1:1/client/v4/".parse().unwrap(),
            SecretString::from("cf-secret-value"),
        );
        let u = c.url("zones", &[("name", "a.com".into())]).unwrap();
        assert_eq!(u.as_str(), "http://127.0.0.1:1/client/v4/zones?name=a.com");
        // The token never shows up in Debug output.
        assert!(!format!("{c:?}").contains("cf-secret-value"));
    }
}
