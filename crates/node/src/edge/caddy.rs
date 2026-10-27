//! Caddy as the node's edge. We always load a *complete* config (never patch),
//! so what runs is exactly what we rendered.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use modula_proto::node::v1::TlsMode;
use serde_json::{Value, json};

use super::ask::Allowlist;
use super::http::{self, AdminAddr, post_json};
use super::rules;
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
/// The access logger of app routes (`http.log.access.<name>`).
const ACCESS_LOGGER: &str = "modula";

/// What a render may use besides the routes.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Where access log lines go (`host:port` of the node's collector);
    /// None = no access log.
    pub access_log: Option<String>,
    /// Rule actions whose Caddy module is missing (left out).
    pub unsupported: BTreeSet<String>,
}

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
    access_log: Option<String>,
    unsupported: Mutex<BTreeSet<String>>,
    last: Mutex<Option<(Value, Instant)>>,
}

impl CaddyEdge {
    pub fn new(
        admin: &str,
        ask_url: String,
        allowlist: Allowlist,
        local_routes: Vec<ResolvedRoute>,
        listeners: Listeners,
        access_log: Option<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            admin: AdminAddr::parse(admin)?,
            listeners,
            ask_url,
            allowlist,
            local_routes,
            hidden: Mutex::new(Vec::new()),
            access_log,
            unsupported: Mutex::new(BTreeSet::new()),
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

/// A handler adding `key` to the request's access log line once the rest
/// of the chain has run (so upstream placeholders are set).
fn log_append(key: &str, value: &str) -> Value {
    json!({ "handler": "log_append", "key": key, "value": value })
}

pub fn render(
    cfg: &EdgeConfig,
    listeners: Listeners,
    admin_listen: &str,
    ask_url: &str,
    opts: &Options,
) -> Value {
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
            let mut proxy = json!({
                "handler": "reverse_proxy",
                "upstreams": ups.iter().map(|d| json!({ "dial": d })).collect::<Vec<_>>(),
                // Stream responses (SSE, long polling) instead of buffering.
                "flush_interval": -1
            });
            // Replicas (on this and other servers): spread requests, skip
            // one that refuses connections for a while, retry on another.
            if ups.len() > 1 {
                proxy["load_balancing"] = json!({
                    "selection_policy": { "policy": "round_robin" },
                    "try_duration": "5s",
                    "try_interval": "250ms"
                });
                proxy["health_checks"] = json!({
                    "passive": { "fail_duration": "10s", "max_fails": 1 }
                });
            }
            let site = cfg.sites.get(host);
            // Sleeping: the upstream is the wake proxy, told which
            // deployment to wake. One connection per request: a woken one
            // is spliced to the container, never reused for another site.
            if let Some(wake) = site.and_then(|s| s.wake.as_deref()) {
                proxy["headers"] = json!({ "request": { "set": { super::wake::HEADER: [wake] } } });
                proxy["transport"] =
                    json!({ "protocol": "http", "keep_alive": { "enabled": false } });
            }
            let mut handle = Vec::new();
            if let (Some(site), Some(_)) = (site, &opts.access_log) {
                handle.push(log_append("deployment_id", &site.deployment_id));
                handle.push(log_append("proxied", if site.proxied { "1" } else { "0" }));
                handle.push(log_append(
                    "upstream",
                    "{http.reverse_proxy.upstream.hostport}",
                ));
            }
            match site {
                Some(s) if !s.rules.is_empty() => handle.push(rules::subroute(
                    &s.deployment_id,
                    &s.rules,
                    s.proxied,
                    &proxy,
                    &opts.unsupported,
                )),
                _ => handle.push(proxy),
            }
            json!({
                "match": [{ "host": [host] }],
                "handle": handle,
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
    let sites: Vec<_> = cfg
        .sites
        .iter()
        .filter(|(h, _)| by_host.contains_key(*h))
        .map(|(_, s)| s)
        .collect();
    if sites.iter().any(|s| s.proxied) {
        // The client of a request from Cloudflare is its CF-Connecting-IP
        // (`{http.vars.client_ip}`, the `client_ip` matcher); from anywhere
        // else the header is ignored.
        server["trusted_proxies"] =
            json!({ "source": "static", "ranges": rules::CLOUDFLARE_RANGES });
        server["client_ip_headers"] = json!(["CF-Connecting-IP"]);
    }
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
    let access = opts.access_log.as_deref().filter(|_| !sites.is_empty());
    if access.is_some() {
        server["logs"] = json!({ "default_logger_name": ACCESS_LOGGER });
    }
    servers.insert("modula".into(), server);
    apps.insert(
        "http".into(),
        json!({ "grace_period": GRACE_PERIOD, "servers": servers }),
    );
    let mut config = json!({
        // Re-declared on every load so Caddy never falls back to localhost:2019.
        "admin": { "listen": admin_listen, "config": { "persist": false } },
        "apps": apps
    });
    if let Some(addr) = access {
        // Access lines go to the node only (not Caddy's own log). With
        // soft_start a node that isn't listening never fails the load;
        // Caddy then writes the lines to stderr until it can reconnect.
        let logger = format!("http.log.access.{ACCESS_LOGGER}");
        config["logging"] = json!({ "logs": {
            "default": { "exclude": [logger] },
            "modula_access": {
                "writer": { "output": "net", "address": addr, "soft_start": true },
                "encoder": { "format": "json" },
                "include": [logger]
            }
        } });
    }
    config
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

        // A Caddy without a rule's plugin rejects the whole config: drop
        // that rule type (reported to the server) and load the rest.
        for _ in 0..3 {
            let opts = Options {
                access_log: self.access_log.clone(),
                unsupported: self.unsupported.lock().expect("edge lock").clone(),
            };
            let rendered = render(
                cfg,
                self.listeners,
                &self.admin.caddy_listen(),
                &self.ask_url,
                &opts,
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
            match self.load(&rendered).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let missing = rules::missing_action(&format!("{e:#}"));
                    let new = missing.is_some_and(|a| {
                        self.unsupported
                            .lock()
                            .expect("edge lock")
                            .insert(a.to_owned())
                    });
                    if !new {
                        return Err(e);
                    }
                    tracing::warn!(
                        action = missing.unwrap_or_default(),
                        "the edge has no module for this rule type; its rules are not applied"
                    );
                }
            }
        }
        anyhow::bail!("the edge config could not be loaded")
    }

    async fn in_flight(&self) -> std::collections::HashMap<std::net::SocketAddr, u64> {
        #[derive(serde::Deserialize)]
        struct Upstream {
            address: String,
            num_requests: u64,
        }
        let Ok(r) = http::get(&self.admin, "/reverse_proxy/upstreams").await else {
            return Default::default();
        };
        serde_json::from_str::<Vec<Upstream>>(&r.body)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|u| Some((u.address.parse().ok()?, u.num_requests)))
            .collect()
    }

    fn unsupported(&self) -> Vec<String> {
        self.unsupported
            .lock()
            .expect("edge lock")
            .iter()
            .cloned()
            .collect()
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
            sites: BTreeMap::new(),
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
            &Options::default(),
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
        // Several replicas: balanced, with failed ones skipped for a while.
        let lb = &routes[0]["handle"][0];
        assert_eq!(
            lb["load_balancing"]["selection_policy"]["policy"],
            "round_robin"
        );
        assert_eq!(lb["health_checks"]["passive"]["fail_duration"], "10s");
        assert!(
            c["apps"]["http"]["servers"]["modula"]["routes"][0]["handle"][0]
                .get("load_balancing")
                .is_none()
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
            hostname: "203.0.113.10".into(),
            upstream: "127.0.0.1:7100".parse().unwrap(),
        }];
        let out = with_local_routes(&c, &local, &[]);
        let dash: Vec<_> = out
            .routes
            .iter()
            .filter(|r| r.upstream.port() == 7100)
            .map(|r| r.hostname.as_str())
            .collect();
        assert_eq!(dash, ["deploy.example.com", "203.0.113.10"]);
        // Not a dashboard node: the names mean nothing here.
        assert_eq!(with_local_routes(&c, &[], &[]).routes.len(), 3);
        // Private mode hides the dashboard under every name.
        let hidden = ["203.0.113.10".to_owned(), "deploy.example.com".to_owned()];
        assert_eq!(with_local_routes(&c, &local, &hidden).routes.len(), 3);
    }

    #[test]
    fn local_modes() {
        let off = render(
            &cfg(TlsMode::TlsOff),
            Listeners::Ports,
            "localhost:2019",
            "x",
            &Options::default(),
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
            &Options::default(),
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
                let c = render(
                    &cfg(tls),
                    listeners,
                    "localhost:2019",
                    "x",
                    &Options::default(),
                );
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
            &Options::default(),
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

        let off = render(
            &cfg(TlsMode::TlsOff),
            Listeners::SystemdSockets,
            "x",
            "x",
            &Options::default(),
        );
        let servers = off["apps"]["http"]["servers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers["modula"]["listen"], json!(["fd/3"]));

        // Without sockets nothing changes: one server on :80 and :443.
        let ports = render(
            &cfg(TlsMode::TlsAcme),
            Listeners::Ports,
            "x",
            "x",
            &Options::default(),
        );
        let servers = ports["apps"]["http"]["servers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers["modula"]["listen"], json!([":80", ":443"]));
    }

    /// The test config with sites for its app hosts: the first proxied by
    /// Cloudflare with every rule type, the IP host plain.
    fn with_sites(tls: TlsMode) -> EdgeConfig {
        let mut c = cfg(tls);
        c.sites.insert(
            "api.apps.example.com".into(),
            super::super::Site {
                deployment_id: "dep1".into(),
                proxied: true,
                rules: rules::tests::every_rule(),
                wake: None,
            },
        );
        c.sites.insert(
            "10.0.0.1".into(),
            super::super::Site {
                deployment_id: "dep2".into(),
                proxied: false,
                rules: vec![],
                wake: None,
            },
        );
        c
    }

    fn logged() -> Options {
        Options {
            access_log: Some("127.0.0.1:7103".into()),
            unsupported: BTreeSet::new(),
        }
    }

    #[test]
    fn access_log_goes_to_the_node() {
        let c = render(
            &with_sites(TlsMode::TlsAcme),
            Listeners::Ports,
            "x",
            "x",
            &logged(),
        );
        assert_eq!(
            c["logging"],
            json!({ "logs": {
                "default": { "exclude": ["http.log.access.modula"] },
                "modula_access": {
                    "writer": { "output": "net", "address": "127.0.0.1:7103", "soft_start": true },
                    "encoder": { "format": "json" },
                    "include": ["http.log.access.modula"]
                }
            } })
        );
        let server = &c["apps"]["http"]["servers"]["modula"];
        assert_eq!(server["logs"]["default_logger_name"], "modula");
        // Each site's lines say which deployment served them, and how.
        let routes = server["routes"].as_array().unwrap();
        let ip = &routes[0]["handle"];
        assert_eq!(
            ip[0],
            json!({ "handler": "log_append", "key": "deployment_id", "value": "dep2" })
        );
        assert_eq!(ip[1]["value"], "0");
        assert_eq!(ip[2]["value"], "{http.reverse_proxy.upstream.hostport}");
        assert_eq!(ip[3]["handler"], "reverse_proxy");
        let api = &routes[1]["handle"];
        assert_eq!(api[1]["value"], "1");
        assert_eq!(api[3]["handler"], "subroute");
        // Proxied sites: CF-Connecting-IP is believed from Cloudflare only.
        assert_eq!(server["trusted_proxies"]["source"], "static");
        assert_eq!(
            server["trusted_proxies"]["ranges"],
            json!(rules::CLOUDFLARE_RANGES)
        );
        assert_eq!(server["client_ip_headers"], json!(["CF-Connecting-IP"]));
        // The 404 fallback carries nothing.
        assert_eq!(routes[2]["handle"][0]["status_code"], 404);
    }

    /// A sleeping site dials the wake proxy and names its deployment; its
    /// requests are still logged (they count as activity).
    #[test]
    fn a_sleeping_site_goes_through_the_wake_proxy() {
        let mut c = with_sites(TlsMode::TlsAcme);
        c.routes.retain(|r| r.hostname != "10.0.0.1");
        c.routes.push(ResolvedRoute {
            hostname: "10.0.0.1".into(),
            upstream: "127.0.0.1:7104".parse().unwrap(),
        });
        c.sites.get_mut("10.0.0.1").unwrap().wake = Some("dep2 80 key".into());
        let out = render(&c, Listeners::Ports, "x", "x", &logged());
        let routes = out["apps"]["http"]["servers"]["modula"]["routes"]
            .as_array()
            .unwrap();
        let ip = &routes[0]["handle"];
        assert_eq!(ip[0]["value"], "dep2");
        let proxy = &ip[3];
        assert_eq!(proxy["upstreams"], json!([{ "dial": "127.0.0.1:7104" }]));
        assert_eq!(
            proxy["headers"]["request"]["set"]["X-Modula-Wake"],
            json!(["dep2 80 key"])
        );
        assert_eq!(proxy["transport"]["keep_alive"]["enabled"], false);
        // The awake site is untouched.
        let api = &routes[1]["handle"][3];
        assert!(api.to_string().contains("10.88.0.2:8080"));
        assert!(!api.to_string().contains("X-Modula-Wake"));
    }

    #[test]
    fn no_sites_no_access_log() {
        // Only local routes (the dashboard): nothing to log, nothing trusted.
        let c = render(
            &cfg(TlsMode::TlsAcme),
            Listeners::Ports,
            "x",
            "x",
            &logged(),
        );
        assert!(c.get("logging").is_none());
        let server = &c["apps"]["http"]["servers"]["modula"];
        assert!(server.get("logs").is_none());
        assert!(server.get("trusted_proxies").is_none());
        assert_eq!(server["routes"][1]["handle"][0]["handler"], "reverse_proxy");
        // A site with logging off still gets its rules.
        let c = render(
            &with_sites(TlsMode::TlsAcme),
            Listeners::Ports,
            "x",
            "x",
            &Options::default(),
        );
        assert!(c.get("logging").is_none());
        assert_eq!(
            c["apps"]["http"]["servers"]["modula"]["routes"][1]["handle"][0]["handler"],
            "subroute"
        );
    }

    /// Every variant through `caddy validate` (which provisions every module)
    /// when MODULA_CADDY_BIN points at a Caddy built like the release's.
    #[test]
    fn rendered_configs_pass_caddy_validate() {
        let Ok(bin) = std::env::var("MODULA_CADDY_BIN") else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("modula-caddy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for tls in [TlsMode::TlsAcme, TlsMode::TlsInternal, TlsMode::TlsOff] {
            for listeners in [Listeners::Ports, Listeners::SystemdSockets] {
                for (opts, sleeping) in [
                    (Options::default(), false),
                    (logged(), false),
                    (logged(), true),
                ] {
                    let mut sites = with_sites(tls);
                    if sleeping {
                        for s in sites.sites.values_mut() {
                            s.wake = Some(format!("{} 80 k", s.deployment_id));
                        }
                    }
                    let c = render(
                        &sites,
                        listeners,
                        "localhost:2999",
                        "http://127.0.0.1:7102/ask",
                        &opts,
                    );
                    let file = dir.join("caddy.json");
                    std::fs::write(&file, serde_json::to_vec(&c).unwrap()).unwrap();
                    let out = std::process::Command::new(&bin)
                        .arg("validate")
                        .arg("--config")
                        .arg(&file)
                        .output()
                        .unwrap();
                    assert!(
                        out.status.success(),
                        "{tls:?} {listeners:?}: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
