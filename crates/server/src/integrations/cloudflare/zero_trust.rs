//! Cloudflare Tunnel and Access (Zero Trust) calls, for the private dashboard
//! (`crate::private`). Needs two extra token permissions:
//! Account · Cloudflare Tunnel · Edit and Account · Access: Apps and Policies · Edit
//! (service tokens are covered by the latter).
//!
//! Everything Modula creates here is named `Modula …` so it is recognisable in
//! the Cloudflare dashboard.

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::client::{Client, DnsRecord, Error, MANAGED_COMMENT, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct Tunnel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `inactive`, `degraded`, `healthy` or `down`.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub connections: Vec<Value>,
}

impl Tunnel {
    pub fn is_healthy(&self) -> bool {
        self.status == "healthy" || (self.status != "down" && !self.connections.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccessApp {
    pub id: String,
    /// The Application Audience (AUD) tag Access puts in its JWTs.
    #[serde(default)]
    pub aud: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceToken {
    pub id: String,
    pub client_id: String,
    /// Only returned when the token is created.
    #[serde(default)]
    pub client_secret: String,
}

/// One Access policy, written inline with its application.
#[derive(Debug, Clone, Serialize)]
pub struct Policy {
    pub name: String,
    /// `allow`, `non_identity` (service tokens) or `bypass`.
    pub decision: &'static str,
    pub include: Vec<Value>,
    pub precedence: u32,
}

impl Policy {
    /// People signing in with an allowed email address or email domain.
    pub fn people(emails: &[String], domains: &[String], precedence: u32) -> Self {
        let include = emails
            .iter()
            .map(|e| json!({ "email": { "email": e } }))
            .chain(
                domains
                    .iter()
                    .map(|d| json!({ "email_domain": { "domain": d } })),
            )
            .collect();
        Self {
            name: "Modula admins".into(),
            decision: "allow",
            include,
            precedence,
        }
    }

    /// Any service token of the account (CLI and CI use one).
    pub fn any_service_token(precedence: u32) -> Self {
        Self {
            name: "Modula CLI and CI (service tokens)".into(),
            decision: "non_identity",
            include: vec![json!({ "any_valid_service_token": {} })],
            precedence,
        }
    }

    /// Exactly one service token.
    pub fn service_token(token_id: &str, precedence: u32) -> Self {
        Self {
            name: "Modula servers".into(),
            decision: "non_identity",
            include: vec![json!({ "service_token": { "token_id": token_id } })],
            precedence,
        }
    }

    /// Everyone, without signing in (endpoints with their own authentication).
    pub fn bypass(precedence: u32) -> Self {
        Self {
            name: "Public endpoints".into(),
            decision: "bypass",
            include: vec![json!({ "everyone": {} })],
            precedence,
        }
    }
}

/// A self-hosted Access application.
#[derive(Debug, Clone)]
pub struct NewApp {
    pub name: String,
    /// `host` or `host/path` entries; the first is the app's main domain.
    pub destinations: Vec<String>,
    pub policies: Vec<Policy>,
}

impl Client {
    fn tunnels_path(account: &str) -> String {
        format!("accounts/{account}/cfd_tunnel")
    }

    /// Whether the token may manage tunnels in `account` (lists at most one).
    pub async fn tunnel_access(&self, account: &str) -> Result<bool> {
        forbidden_is_false(
            self.call::<Value>(
                Method::GET,
                &Self::tunnels_path(account),
                &[("per_page", "1".into())],
                None,
            )
            .await,
        )
    }

    /// Whether the token may manage Access applications in `account`.
    pub async fn access_apps_access(&self, account: &str) -> Result<bool> {
        forbidden_is_false(
            self.call::<Value>(
                Method::GET,
                &format!("accounts/{account}/access/apps"),
                &[("per_page", "1".into())],
                None,
            )
            .await,
        )
    }

    /// The account's Zero Trust team domain (`<team>.cloudflareaccess.com`),
    /// or None when Zero Trust was never set up.
    pub async fn team_domain(&self, account: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Org {
            #[serde(default)]
            auth_domain: String,
        }
        match self
            .get_one::<Org>(&format!("accounts/{account}/access/organizations"))
            .await
        {
            Ok(o) if !o.auth_domain.is_empty() => Ok(Some(o.auth_domain)),
            Ok(_) => Ok(None),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A tunnel whose configuration lives at Cloudflare (`config_src: cloudflare`).
    pub async fn create_tunnel(&self, account: &str, name: &str) -> Result<Tunnel> {
        let (t, _) = self
            .call::<Tunnel>(
                Method::POST,
                &Self::tunnels_path(account),
                &[],
                Some(&json!({ "name": name, "config_src": "cloudflare" })),
            )
            .await?;
        t.ok_or_else(|| Error::Decode("create tunnel: empty result".into()))
    }

    pub async fn tunnel(&self, account: &str, id: &str) -> Result<Tunnel> {
        self.get_one(&format!("{}/{id}", Self::tunnels_path(account)))
            .await
    }

    /// The token `cloudflared tunnel run --token` needs.
    pub async fn tunnel_token(&self, account: &str, id: &str) -> Result<String> {
        self.get_one(&format!("{}/{id}/token", Self::tunnels_path(account)))
            .await
    }

    /// Replaces the tunnel's ingress rules: `(hostname, service)` pairs, then
    /// a catch-all 404.
    pub async fn configure_tunnel(
        &self,
        account: &str,
        id: &str,
        ingress: &[(String, String)],
    ) -> Result<()> {
        let mut rules: Vec<Value> = ingress
            .iter()
            .map(|(host, service)| json!({ "hostname": host, "service": service }))
            .collect();
        rules.push(json!({ "service": "http_status:404" }));
        self.call::<Value>(
            Method::PUT,
            &format!("{}/{id}/configurations", Self::tunnels_path(account)),
            &[],
            Some(&json!({ "config": { "ingress": rules } })),
        )
        .await?;
        Ok(())
    }

    /// Deletes a tunnel (dropping its connections first). Gone already is fine.
    pub async fn delete_tunnel(&self, account: &str, id: &str) -> Result<()> {
        let base = format!("{}/{id}", Self::tunnels_path(account));
        ignore_404(
            self.call::<Value>(Method::DELETE, &format!("{base}/connections"), &[], None)
                .await,
        )?;
        ignore_404(self.call::<Value>(Method::DELETE, &base, &[], None).await)
    }

    pub async fn create_access_app(&self, account: &str, app: &NewApp) -> Result<AccessApp> {
        let domain = app
            .destinations
            .first()
            .ok_or_else(|| Error::Decode("an Access app needs a destination".into()))?;
        let body = json!({
            "name": app.name,
            "type": "self_hosted",
            "domain": domain,
            "destinations": app.destinations.iter()
                .map(|d| json!({ "type": "public", "uri": d }))
                .collect::<Vec<_>>(),
            "session_duration": "24h",
            "app_launcher_visible": false,
            "policies": app.policies,
        });
        let (a, _) = self
            .call::<AccessApp>(
                Method::POST,
                &format!("accounts/{account}/access/apps"),
                &[],
                Some(&body),
            )
            .await?;
        a.ok_or_else(|| Error::Decode("create Access app: empty result".into()))
    }

    pub async fn delete_access_app(&self, account: &str, id: &str) -> Result<()> {
        ignore_404(
            self.call::<Value>(
                Method::DELETE,
                &format!("accounts/{account}/access/apps/{id}"),
                &[],
                None,
            )
            .await,
        )
    }

    /// A service token valid for a year (the secret is in the result only now).
    pub async fn create_service_token(&self, account: &str, name: &str) -> Result<ServiceToken> {
        let (t, _) = self
            .call::<ServiceToken>(
                Method::POST,
                &format!("accounts/{account}/access/service_tokens"),
                &[],
                Some(&json!({ "name": name, "duration": "8760h" })),
            )
            .await?;
        t.ok_or_else(|| Error::Decode("create service token: empty result".into()))
    }

    pub async fn delete_service_token(&self, account: &str, id: &str) -> Result<()> {
        ignore_404(
            self.call::<Value>(
                Method::DELETE,
                &format!("accounts/{account}/access/service_tokens/{id}"),
                &[],
                None,
            )
            .await,
        )
    }

    /// Every record named exactly `name`, of any type.
    pub async fn records_named(&self, zone_id: &str, name: &str) -> Result<Vec<DnsRecord>> {
        self.list(
            &format!("zones/{zone_id}/dns_records"),
            &[("name", name.to_owned())],
        )
        .await
    }

    /// Creates a record as given (the user's own, when restoring one).
    pub async fn create_record(
        &self,
        zone_id: &str,
        kind: &str,
        name: &str,
        content: &str,
        proxied: Option<bool>,
        comment: Option<&str>,
    ) -> Result<()> {
        let mut body = json!({ "type": kind, "name": name, "content": content, "ttl": 1 });
        if let Some(p) = proxied {
            body["proxied"] = json!(p);
        }
        if let Some(c) = comment {
            body["comment"] = json!(c);
        }
        self.call::<Value>(
            Method::POST,
            &format!("zones/{zone_id}/dns_records"),
            &[],
            Some(&body),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_record(&self, zone_id: &str, id: &str) -> Result<()> {
        ignore_404(
            self.call::<Value>(
                Method::DELETE,
                &format!("zones/{zone_id}/dns_records/{id}"),
                &[],
                None,
            )
            .await,
        )
    }

    /// Points `name` at a tunnel: a proxied CNAME marked as Modula's.
    pub async fn route_to_tunnel(&self, zone_id: &str, name: &str, tunnel_id: &str) -> Result<()> {
        let target = format!("{tunnel_id}.cfargotunnel.com");
        let existing = self.records_named(zone_id, name).await?;
        if existing.iter().any(|r| {
            r.kind == "CNAME" && r.content.eq_ignore_ascii_case(&target) && r.proxied == Some(true)
        }) {
            return Ok(());
        }
        self.create_record(
            zone_id,
            "CNAME",
            name,
            &target,
            Some(true),
            Some(MANAGED_COMMENT),
        )
        .await
    }
}

fn forbidden_is_false<T>(r: Result<T>) -> Result<bool> {
    match r {
        Ok(_) => Ok(true),
        Err(e) if e.is_forbidden() => Ok(false),
        Err(e) => Err(e),
    }
}

fn ignore_404<T>(r: Result<T>) -> Result<()> {
    match r {
        Ok(_) => Ok(()),
        Err(e) if e.status() == Some(404) => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_serialize_as_cloudflare_expects() {
        let p = Policy::people(&["a@x.com".into()], &["x.com".into()], 1);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["decision"], "allow");
        assert_eq!(v["include"][0]["email"]["email"], "a@x.com");
        assert_eq!(v["include"][1]["email_domain"]["domain"], "x.com");
        let v = serde_json::to_value(Policy::service_token("t1", 1)).unwrap();
        assert_eq!(v["decision"], "non_identity");
        assert_eq!(v["include"][0]["service_token"]["token_id"], "t1");
    }

    #[test]
    fn tunnel_health() {
        let t = |status: &str, n: usize| Tunnel {
            id: "t".into(),
            name: String::new(),
            status: status.into(),
            connections: vec![json!({}); n],
        };
        assert!(t("healthy", 0).is_healthy());
        assert!(t("degraded", 1).is_healthy());
        assert!(!t("inactive", 0).is_healthy());
        assert!(!t("down", 2).is_healthy());
    }
}
