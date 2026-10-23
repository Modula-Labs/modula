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
/// How often the watchdog checks that Caddy still has our config.
const WATCH_EVERY: Duration = Duration::from_secs(3);
const NOT_FOUND: &str = "No service is deployed at this address.";

pub struct CaddyEdge {
    admin: AdminAddr,
    ask_url: String,
    allowlist: Allowlist,
    /// Routes this node serves on its own, e.g. the dashboard on the main server.
    local_routes: Vec<ResolvedRoute>,
    last: Mutex<Option<(Value, Instant)>>,
}

impl CaddyEdge {
    pub fn new(
        admin: &str,
        ask_url: String,
        allowlist: Allowlist,
        local_routes: Vec<ResolvedRoute>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            admin: AdminAddr::parse(admin)?,
            ask_url,
            allowlist,
            local_routes,
            last: Mutex::new(None),
        })
    }

    /// Re-loads the last config as soon as Caddy comes back empty (it keeps
    /// nothing across restarts: `persist: false`), instead of waiting for the
    /// next reconcile.
    pub async fn watch(self: std::sync::Arc<Self>, stop: tokio_util::sync::CancellationToken) {
        let mut tick = tokio::time::interval(WATCH_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tick.tick() => {}
            }
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
                Err(_) => continue, // Caddy is down; check again once it's up
            };
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

/// The full Caddy JSON config for `cfg`.
pub fn render(cfg: &EdgeConfig, admin_listen: &str, ask_url: &str) -> Value {
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
    let mut server = json!({ "routes": routes });
    let mut apps = serde_json::Map::new();
    match cfg.tls {
        TlsMode::TlsOff => {
            server["listen"] = json!([":80"]);
            server["automatic_https"] = json!({ "disable": true });
        }
        TlsMode::TlsInternal => {
            server["listen"] = json!([":80", ":443"]);
            apps.insert(
                "tls".into(),
                json!({ "automation": { "policies": [{ "subjects": hosts, "issuers": [{ "module": "internal" }] }] } }),
            );
        }
        TlsMode::TlsAcme | TlsMode::TlsUnspecified => {
            server["listen"] = json!([":80", ":443"]);
            let mut issuer = json!({ "module": "acme" });
            if !cfg.acme_email.is_empty() {
                issuer["email"] = json!(cfg.acme_email);
            }
            apps.insert(
                "tls".into(),
                json!({
                    "automation": {
                        "policies": [{ "issuers": [issuer], "on_demand": true }],
                        "on_demand": { "permission": { "module": "http", "endpoint": ask_url } }
                    }
                }),
            );
        }
    }
    apps.insert("http".into(), json!({ "servers": { "modula": server } }));
    json!({
        // Re-declared on every load so Caddy never falls back to localhost:2019.
        "admin": { "listen": admin_listen, "config": { "persist": false } },
        "apps": apps
    })
}

#[async_trait]
impl EdgeSink for CaddyEdge {
    async fn apply(&self, cfg: &EdgeConfig) -> anyhow::Result<()> {
        let mut cfg = cfg.clone();
        cfg.routes.extend(self.local_routes.iter().cloned());
        let cfg = &cfg;
        let hosts: HashSet<String> = cfg
            .routes
            .iter()
            .filter_map(|r| normalize_host(&r.hostname))
            .collect();
        *self.allowlist.write().expect("allowlist lock") = hosts;

        let rendered = render(cfg, &self.admin.caddy_listen(), &self.ask_url);
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
        }
    }

    #[test]
    fn acme_config_is_gated_by_ask() {
        let c = render(
            &cfg(TlsMode::TlsAcme),
            "unix//run/caddy/admin.sock",
            "http://127.0.0.1:7102/ask",
        );
        assert_eq!(c["admin"]["listen"], "unix//run/caddy/admin.sock");
        assert_eq!(c["admin"]["config"]["persist"], false);
        let routes = c["apps"]["http"]["servers"]["modula"]["routes"]
            .as_array()
            .unwrap();
        // One normalized host with both upstreams; the IP literal is dropped; then the 404.
        assert_eq!(routes.len(), 2);
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
        assert_eq!(
            auto["policies"][0]["issuers"][0]["email"],
            "ops@example.com"
        );
    }

    #[test]
    fn local_modes() {
        let off = render(&cfg(TlsMode::TlsOff), "localhost:2019", "x");
        assert_eq!(
            off["apps"]["http"]["servers"]["modula"]["listen"],
            json!([":80"])
        );
        assert!(off["apps"].get("tls").is_none());
        let internal = render(&cfg(TlsMode::TlsInternal), "localhost:2019", "x");
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
            let c = render(&cfg(tls), "localhost:2019", "x");
            let mut proxies = 0;
            walk(&c["apps"]["http"], &mut proxies);
            assert!(proxies > 0);
        }
    }
}
