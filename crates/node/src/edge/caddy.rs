//! Caddy as the node's edge. We always load a *complete* config (never patch),
//! so what runs is exactly what we rendered.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use modula_proto::node::v1::TlsMode;
use serde_json::{Value, json};

use super::ask::Allowlist;
use super::http::{self, AdminAddr, post_json};
use super::{EdgeConfig, EdgeSink, ResolvedRoute, normalize_host};

/// Re-load even an unchanged config this often: it restores our routes after
/// Caddy restarts, and Caddy skips identical configs without a reload.
const REFRESH_EVERY: Duration = Duration::from_secs(30);
/// How often the watchdog asks Caddy whether it still has our config (each
/// question is a line in Caddy's log).
const CHECK_EVERY: Duration = Duration::from_secs(3);
/// How often it looks at the admin socket: Caddy creates it anew when it
/// starts, so a restart is noticed (and the config restored) within this,
/// while connections wait in the kernel's backlog (socket activation).
const WATCH_EVERY: Duration = Duration::from_millis(100);
/// How long Caddy lets in-flight requests finish when it stops or reloads.
const GRACE_PERIOD: &str = "10s";
const NOT_FOUND: &str = "No service is deployed at this address.";

/// Where Caddy's HTTP servers listen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Listeners {
    /// Caddy binds :80 and :443 itself (development, or no socket unit).
    #[default]
    Ports,
    /// systemd holds the sockets (modula-caddy.socket) and passes them in this
    /// order: fd 3 = TCP 80, fd 4 = TCP 443, fd 5 = UDP 443 (HTTP/3). The ports
    /// stay open while Caddy restarts; connections wait in the backlog.
    SystemdSockets,
}

pub struct CaddyEdge {
    admin: AdminAddr,
    listeners: Listeners,
    ask_url: String,
    allowlist: Allowlist,
    /// Routes this node serves on its own, e.g. the dashboard on the main server.
    local_routes: Vec<ResolvedRoute>,
    /// Local routes not to serve (private mode hides the dashboard).
    hidden: Mutex<Vec<String>>,
    last: Mutex<Option<(Value, Instant)>>,
}

impl CaddyEdge {
    pub fn new(
        admin: &str,
        ask_url: String,
        allowlist: Allowlist,
        local_routes: Vec<ResolvedRoute>,
        listeners: Listeners,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            admin: AdminAddr::parse(admin)?,
            listeners,
            ask_url,
            allowlist,
            local_routes,
            hidden: Mutex::new(Vec::new()),
            last: Mutex::new(None),
        })
    }

    /// Re-loads the last config as soon as Caddy comes back empty (it keeps
    /// nothing across restarts: `persist: false`), instead of waiting for the
    /// next reconcile.
    pub async fn watch(self: std::sync::Arc<Self>, stop: tokio_util::sync::CancellationToken) {
        let mut socket = self.admin.socket_identity();
        let mut checked = Instant::now();
        let mut down = false;
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(WATCH_EVERY) => {}
            }
            let now = self.admin.socket_identity();
            let restarted = now != socket;
            socket = now;
            if !restarted && !down && checked.elapsed() < CHECK_EVERY {
                continue;
            }
            checked = Instant::now();
            let Some(config) = self
                .last
                .lock()
                .expect("edge lock")
                .as_ref()
                .map(|(c, _)| c.clone())
            else {
                continue;
            };
            let missing = match http::get(&self.admin, "/config/apps/http/servers/modula").await {
                Ok(r) => r.status != 200 || r.body.trim() == "null",
                Err(_) => {
                    // Caddy is down; check again soon and reload once it's up.
                    down = true;
                    continue;
                }
            };
            down = false;
            if missing {
                match self.load(&config).await {
                    Ok(()) => tracing::info!("restored the edge config after a Caddy restart"),
                    Err(e) => tracing::warn!(
                        error = format!("{e:#}"),
                        "could not restore the edge config"
                    ),
                }
            }
        }
    }

    async fn load(&self, rendered: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_vec(rendered)?;
        let resp = post_json(&self.admin, "/load", &body).await?;
        if resp.status != 200 {
            anyhow::bail!(
                "Caddy rejected the config ({}): {}",
                resp.status,
                resp.body.chars().take(500).collect::<String>()
            );
        }
        *self.last.lock().expect("edge lock") = Some((rendered.clone(), Instant::now()));
        Ok(())
    }
}

/// `cfg` plus this node's own routes (not hidden), and the dashboard's
/// domain names on the same upstream as its own route.
fn with_local_routes(cfg: &EdgeConfig, local: &[ResolvedRoute], hidden: &[String]) -> EdgeConfig {
    let mut cfg = cfg.clone();
    let local: Vec<&ResolvedRoute> = local
        .iter()
        .filter(|r| !hidden.contains(&r.hostname))
        .collect();
    if let Some(dashboard) = local.first() {
        for h in cfg.dashboard_hosts.iter().filter_map(|h| normalize_host(h)) {
            if !hidden.contains(&h) {
                cfg.routes.push(ResolvedRoute {
                    hostname: h,
                    upstream: dashboard.upstream,
                });
            }
        }
    }
    cfg.routes.extend(local.into_iter().cloned());
    cfg
}

/// The full Caddy JSON config for `cfg`.
/// Plain-HTTP routes: every known hostname redirects to HTTPS (308 keeps
/// the method and body); anything else is the usual 404.
fn https_redirects(hosts: &[&String]) -> Value {
    let mut routes = Vec::new();
    if !hosts.is_empty() {
        routes.push(json!({
            "match": [{ "host": hosts }],
            "handle": [{
                "handler": "static_response",
                "status_code": 308,
                "headers": { "Location": ["https://{http.request.host}{http.request.uri}"] }
            }],
            "terminal": true
        }));
    }
    routes.push(json!({
        "handle": [{ "handler": "static_response", "status_code": 404, "body": NOT_FOUND }]
    }));
    Value::Array(routes)
}

pub fn render(cfg: &EdgeConfig, listeners: Listeners, admin_listen: &str, ask_url: &str) -> Value {
    // One route per hostname; several upstreams if several replicas serve it.
    let mut by_host: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in &cfg.routes {
        if let Some(h) = normalize_host(&r.hostname) {
            let dial = r.upstream.to_string();
            let ups = by_host.entry(h).or_default();
            if !ups.contains(&dial) {
                ups.push(dial);
            }
        }
    }
    let mut routes: Vec<Value> = by_host
        .iter()
        .map(|(host, ups)| {
            json!({
                "match": [{ "host": [host] }],
                "handle": [{
                    "handler": "reverse_proxy",
                    "upstreams": ups.iter().map(|d| json!({ "dial": d })).collect::<Vec<_>>(),
                    // Stream responses (SSE, long polling) instead of buffering.
                    "flush_interval": -1
                }],
                "terminal": true
            })
        })
        .collect();
    routes.push(json!({
        "handle": [{ "handler": "static_response", "status_code": 404, "body": NOT_FOUND }]
    }));

    let hosts: Vec<&String> = by_host.keys().collect();
    // A server without a domain is reached by its IP: browsers send no SNI
    // for IP literals, so its certificate is managed up front (not on
    // demand) and is the default for connections without SNI.
    let ips: Vec<&String> = hosts
        .iter()
        .copied()
        .filter(|h| super::ipv4_host(h).is_some())
        .collect();
    let mut server = json!({ "routes": routes });
    let mut servers = serde_json::Map::new();
    let mut apps = serde_json::Map::new();
    match (cfg.tls, listeners) {
        (TlsMode::TlsOff, Listeners::Ports) => {
            server["listen"] = json!([":80"]);
            server["automatic_https"] = json!({ "disable": true });
        }
        (TlsMode::TlsOff, Listeners::SystemdSockets) => {
            server["listen"] = json!(["fd/3"]);
            server["listen_protocols"] = json!([["h1"]]);
            server["automatic_https"] = json!({ "disable": true });
        }
        (_, Listeners::Ports) => server["listen"] = json!([":80", ":443"]),
        (_, Listeners::SystemdSockets) => {
            // Caddy can't tell which descriptor is the HTTP port, so plain
            // HTTP gets a server of its own, and HTTP/3 is bound to the UDP
            // socket explicitly. That server only redirects to HTTPS (what
            // automatic HTTPS does on :80): nothing is served in clear text.
            server["listen"] = json!(["fd/4", "fdgram/5"]);
            server["listen_protocols"] = json!([["h1", "h2"], ["h3"]]);
            server["automatic_https"] = json!({ "disable_redirects": true });
            let mut plain = server.clone();
            plain["routes"] = https_redirects(&hosts);
            plain["listen"] = json!(["fd/3"]);
            plain["listen_protocols"] = json!([["h1"]]);
            // ACME HTTP-01 challenges are still answered here (every server does).
            plain["automatic_https"] = json!({ "disable": true });
            servers.insert("modula_http".into(), plain);
        }
    }
    match cfg.tls {
        TlsMode::TlsOff => {}
        TlsMode::TlsInternal => {
            apps.insert(
                "tls".into(),
                json!({ "automation": { "policies": [{ "subjects": hosts, "issuers": [{ "module": "internal" }] }] } }),
            );
        }
        TlsMode::TlsAcme | TlsMode::TlsUnspecified => {
            let mut issuer = json!({ "module": "acme" });
            if !cfg.acme_email.is_empty() {
                issuer["email"] = json!(cfg.acme_email);
            }
            let mut policies = Vec::new();
            let mut tls = serde_json::Map::new();
            if !ips.is_empty() {
                // Let's Encrypt issues IP certificates only with its 6-day
                // profile (renewed automatically). Should that fail, Caddy's
                // own CA keeps the connection encrypted (a browser warning
                // instead of no dashboard).
                let mut short = issuer.clone();
                short["profile"] = json!("shortlived");
                policies.push(json!({
                    "subjects": ips,
                    "issuers": [short, { "module": "internal" }]
                }));
                tls.insert("certificates".into(), json!({ "automate": ips }));
                server["tls_connection_policies"] = json!([{ "default_sni": ips[0] }]);
            }
            policies.push(json!({ "issuers": [issuer], "on_demand": true }));
            tls.insert(
                "automation".into(),
                json!({
                    "policies": policies,
                    "on_demand": { "permission": { "module": "http", "endpoint": ask_url } }
                }),
            );
            apps.insert("tls".into(), Value::Object(tls));
        }
    }
    servers.insert("modula".into(), server);
    apps.insert(
        "http".into(),
        json!({ "grace_period": GRACE_PERIOD, "servers": servers }),
    );
    json!({
        // Re-declared on every load so Caddy never falls back to localhost:2019.
        "admin": { "listen": admin_listen, "config": { "persist": false } },
        "apps": apps
    })
}

#[async_trait]
impl EdgeSink for CaddyEdge {
    fn set_hidden(&self, hosts: Vec<String>) {
        *self.hidden.lock().expect("edge lock") = hosts;
    }

    async fn apply(&self, cfg: &EdgeConfig) -> anyhow::Result<()> {
        let hidden = self.hidden.lock().expect("edge lock").clone();
        let cfg = with_local_routes(cfg, &self.local_routes, &hidden);
        let cfg = &cfg;
        let hosts: HashSet<String> = cfg
            .routes
            .iter()
            .filter_map(|r| normalize_host(&r.hostname))
            .collect();
        *self.allowlist.write().expect("allowlist lock") = hosts;

        let rendered = render(
            cfg,
            self.listeners,
            &self.admin.caddy_listen(),
            &self.ask_url,
        );
        let fresh = {
            let last = self.last.lock().expect("edge lock");
            match &*last {
                Some((prev, at)) => *prev != rendered || at.elapsed() > REFRESH_EVERY,
                None => true,
            }
        };
        if !fresh {
            return Ok(());
        }
        self.load(&rendered).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::ResolvedRoute;

    fn cfg(tls: TlsMode) -> EdgeConfig {
        EdgeConfig {
            routes: vec![
                ResolvedRoute {
                    hostname: "API.apps.example.com".into(),
                    upstream: "10.88.0.2:8080".parse().unwrap(),
                },
                ResolvedRoute {
                    hostname: "api.apps.example.com".into(),
                    upstream: "10.88.0.3:8080".parse().unwrap(),
                },
                ResolvedRoute {
                    hostname: "10.0.0.1".into(),
                    upstream: "10.88.0.4:80".parse().unwrap(),
                },
            ],
            tls,
            acme_email: "ops@example.com".into(),
            dashboard_hosts: vec![],
        }
    }

    #[test]
    fn acme_config_is_gated_by_ask() {
        let c = render(
            &cfg(TlsMode::TlsAcme),
            Listeners::Ports,
            "unix//run/caddy/admin.sock",
            "http://127.0.0.1:7102/ask",
        );
        assert_eq!(c["admin"]["listen"], "unix//run/caddy/admin.sock");
        assert_eq!(c["admin"]["config"]["persist"], false);
        let routes = c["apps"]["http"]["servers"]["modula"]["routes"]
            .as_array()
            .unwrap();
        // The IP host (a server without a domain), one normalized host with
        // both upstreams, then the 404.
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[0]["match"][0]["host"][0], "10.0.0.1");
        let routes = &routes[1..];
        assert_eq!(routes[0]["match"][0]["host"][0], "api.apps.example.com");
        assert_eq!(
            routes[0]["handle"][0]["upstreams"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(routes[1]["handle"][0]["status_code"], 404);
        let auto = &c["apps"]["tls"]["automation"];
        assert_eq!(
            auto["on_demand"]["permission"]["endpoint"],
            "http://127.0.0.1:7102/ask"
        );
        // The IP: a 6-day Let's Encrypt certificate, managed up front, with
        // Caddy's own CA as the fallback, and the default without SNI.
        assert_eq!(auto["policies"][0]["subjects"], json!(["10.0.0.1"]));
        assert_eq!(auto["policies"][0]["issuers"][0]["profile"], "shortlived");
        assert_eq!(auto["policies"][0]["issuers"][1]["module"], "internal");
        assert_eq!(
            c["apps"]["tls"]["certificates"]["automate"],
            json!(["10.0.0.1"])
        );
        assert_eq!(
            c["apps"]["http"]["servers"]["modula"]["tls_connection_policies"][0]["default_sni"],
            "10.0.0.1"
        );
        assert_eq!(auto["policies"][1]["on_demand"], true);
        assert_eq!(
            auto["policies"][1]["issuers"][0]["email"],
            "ops@example.com"
        );
    }

    #[test]
    fn the_dashboard_domain_shares_the_dashboard_route() {
        let mut c = cfg(TlsMode::TlsAcme);
        c.dashboard_hosts = vec!["Deploy.Example.com".into(), "bad host".into()];
        let local = vec![ResolvedRoute {
            hostname: "159.69.253.176".into(),
            upstream: "127.0.0.1:7100".parse().unwrap(),
        }];
        let out = with_local_routes(&c, &local, &[]);
        let dash: Vec<_> = out
            .routes
            .iter()
            .filter(|r| r.upstream.port() == 7100)
            .map(|r| r.hostname.as_str())
            .collect();
        assert_eq!(dash, ["deploy.example.com", "159.69.253.176"]);
        // Not a dashboard node: the names mean nothing here.
        assert_eq!(with_local_routes(&c, &[], &[]).routes.len(), 3);
        // Private mode hides the dashboard under every name.
        let hidden = ["159.69.253.176".to_owned(), "deploy.example.com".to_owned()];
        assert_eq!(with_local_routes(&c, &local, &hidden).routes.len(), 3);
    }

    #[test]
    fn local_modes() {
        let off = render(
            &cfg(TlsMode::TlsOff),
            Listeners::Ports,
            "localhost:2019",
            "x",
        );
        assert_eq!(
            off["apps"]["http"]["servers"]["modula"]["listen"],
            json!([":80"])
        );
        assert!(off["apps"].get("tls").is_none());
        let internal = render(
            &cfg(TlsMode::TlsInternal),
            Listeners::Ports,
            "localhost:2019",
            "x",
        );
        assert_eq!(
            internal["apps"]["tls"]["automation"]["policies"][0]["issuers"][0]["module"],
            "internal"
        );
    }

    /// Server-sent events and long polls (an SSE ping every 10s, 25s polls)
    /// must pass through unbuffered and without short timeouts. Caddy's
    /// defaults are right (no read/write timeouts, 5 min idle), so the
    /// rendered config must not set any timeout of its own.
    #[test]
    fn streams_are_flushed_and_never_timed_out() {
        fn walk(v: &Value, proxies: &mut usize) {
            match v {
                Value::Object(m) => {
                    for (k, v) in m {
                        assert!(!k.contains("timeout"), "unexpected {k}: {v}");
                    }
                    if m.get("handler").and_then(Value::as_str) == Some("reverse_proxy") {
                        *proxies += 1;
                        assert_eq!(m["flush_interval"], -1, "buffered proxy: {v}");
                    }
                    m.values().for_each(|v| walk(v, proxies));
                }
                Value::Array(a) => a.iter().for_each(|v| walk(v, proxies)),
                _ => {}
            }
        }
        for tls in [TlsMode::TlsAcme, TlsMode::TlsInternal, TlsMode::TlsOff] {
            for listeners in [Listeners::Ports, Listeners::SystemdSockets] {
                let c = render(&cfg(tls), listeners, "localhost:2019", "x");
                let mut proxies = 0;
                walk(&c["apps"]["http"], &mut proxies);
                assert!(proxies > 0);
            }
        }
    }

    /// With socket activation (verified against Caddy 2.10.2), HTTPS and
    /// HTTP/3 take fds 4 and 5 and plain HTTP gets its own server on fd 3 with
    /// the same routes: Caddy can't tell an fd's port, and would otherwise
    /// treat fd 3 as TLS and try HTTP/3 on it.
    #[test]
    fn socket_activation_uses_the_systemd_descriptors() {
        let c = render(
            &cfg(TlsMode::TlsAcme),
            Listeners::SystemdSockets,
            "unix//run/caddy/admin.sock",
            "http://127.0.0.1:7102/ask",
        );
        let servers = &c["apps"]["http"]["servers"];
        assert_eq!(servers["modula"]["listen"], json!(["fd/4", "fdgram/5"]));
        assert_eq!(
            servers["modula"]["listen_protocols"],
            json!([["h1", "h2"], ["h3"]])
        );
        assert_eq!(
            servers["modula"]["automatic_https"]["disable_redirects"],
            true
        );
        assert_eq!(servers["modula_http"]["listen"], json!(["fd/3"]));
        assert_eq!(servers["modula_http"]["automatic_https"]["disable"], true);
        // Plain HTTP never proxies: known hosts redirect to HTTPS.
        let plain = servers["modula_http"]["routes"].as_array().unwrap();
        assert_eq!(plain[0]["handle"][0]["status_code"], 308);
        assert_eq!(
            plain[0]["handle"][0]["headers"]["Location"][0],
            "https://{http.request.host}{http.request.uri}"
        );
        assert_eq!(
            plain[0]["match"][0]["host"],
            json!(["10.0.0.1", "api.apps.example.com"])
        );
        assert!(
            !plain
                .iter()
                .any(|r| r["handle"][0]["handler"] == "reverse_proxy")
        );
        assert_eq!(c["apps"]["http"]["grace_period"], GRACE_PERIOD);

        let off = render(&cfg(TlsMode::TlsOff), Listeners::SystemdSockets, "x", "x");
        let servers = off["apps"]["http"]["servers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers["modula"]["listen"], json!(["fd/3"]));

        // Without sockets nothing changes: one server on :80 and :443.
        let ports = render(&cfg(TlsMode::TlsAcme), Listeners::Ports, "x", "x");
        let servers = ports["apps"]["http"]["servers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers["modula"]["listen"], json!([":80", ":443"]));
    }
}
