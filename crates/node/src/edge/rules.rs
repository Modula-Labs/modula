//! A service's edge rules as Caddy routes. The server validated them; here
//! they only become JSON. Every user-supplied string that Caddy would expand
//! placeholders in is escaped, so a rule can never read `{env.*}` or files.

use std::collections::BTreeSet;

use modula_proto::node::v1 as pb;
use pb::edge_condition::Kind;
use pb::edge_rule::Action;
use serde_json::{Map, Value, json};

/// Cloudflare's published edge ranges (cloudflare.com/ips). Only requests
/// from these carry a CF-Connecting-IP / CF-IPCountry we believe.
pub const CLOUDFLARE_RANGES: &[&str] = &[
    "173.245.48.0/20",
    "103.21.244.0/22",
    "103.22.200.0/22",
    "103.31.4.0/22",
    "141.101.64.0/18",
    "108.162.192.0/18",
    "190.93.240.0/20",
    "188.114.96.0/20",
    "197.234.240.0/22",
    "198.41.128.0/17",
    "162.158.0.0/15",
    "104.16.0.0/13",
    "104.24.0.0/14",
    "172.64.0.0/13",
    "131.0.72.0/22",
    "2400:cb00::/32",
    "2606:4700::/32",
    "2803:f800::/32",
    "2405:b500::/32",
    "2405:8100::/32",
    "2a06:98c0::/29",
    "2c0f:f248::/32",
];

/// The action's name, as nodes report unsupported ones.
pub fn action_name(rule: &pb::EdgeRule) -> &'static str {
    match rule.action {
        Some(Action::RateLimit(_)) => "rate_limit",
        Some(Action::Block(_)) => "block",
        Some(Action::Allow(_)) => "allow",
        Some(Action::Redirect(_)) => "redirect",
        Some(Action::Headers(_)) => "headers",
        Some(Action::Cache(_)) => "cache",
        None => "none",
    }
}

/// The action a Caddy "unknown module" error is about, if it is one of
/// the plugin modules rules need (e.g. `unknown module: http.handlers.rate_limit`).
pub fn missing_action(error: &str) -> Option<&'static str> {
    let module = error.split("unknown module: ").nth(1)?;
    let module = module
        .split(|c: char| c.is_whitespace() || c == '"' || c == ',')
        .next()?;
    match module {
        "http.handlers.rate_limit" => Some("rate_limit"),
        "http.handlers.cache" => Some("cache"),
        _ => None,
    }
}

/// `s` with Caddy placeholders disabled: every brace escaped.
pub fn escape(s: &str) -> String {
    s.replace('{', "\\{").replace('}', "\\}")
}

/// A redirect target: `{host}`, `{path}` and `{query}` become Caddy's
/// placeholders, every other brace is literal.
fn redirect_target(t: &str) -> String {
    escape(t)
        .replace("\\{host\\}", "{http.request.host}")
        .replace("\\{path\\}", "{http.request.uri.path}")
        .replace("\\{query\\}", "{http.request.uri.query}")
}

/// Matcher sets ANDed: merged when they use different matchers, otherwise
/// `not(not a OR not b)`.
fn and(a: Map<String, Value>, b: Map<String, Value>) -> Map<String, Value> {
    if a.is_empty() {
        return b;
    }
    if b.is_empty() {
        return a;
    }
    if a.keys().any(|k| b.contains_key(k)) {
        let mut m = Map::new();
        m.insert(
            "not".into(),
            json!([{ "not": [Value::Object(a)] }, { "not": [Value::Object(b)] }]),
        );
        return m;
    }
    let mut a = a;
    a.extend(b);
    a
}

fn condition(c: &pb::EdgeCondition, proxied: bool) -> Map<String, Value> {
    let mut m = Map::new();
    match &c.kind {
        Some(Kind::Path(p)) => {
            let v = match pb::EdgePathOp::try_from(p.op).unwrap_or(pb::EdgePathOp::PathMatches) {
                pb::EdgePathOp::PathPrefix if !p.value.ends_with('*') => format!("{}*", p.value),
                _ => p.value.clone(),
            };
            m.insert("path".into(), json!([v]));
        }
        Some(Kind::Method(l)) => {
            m.insert("method".into(), json!(l.values));
        }
        Some(Kind::Header(h)) => {
            let values =
                match pb::EdgeHeaderOp::try_from(h.op).unwrap_or(pb::EdgeHeaderOp::HeaderExists) {
                    // An empty list: the header is present, with any value.
                    pb::EdgeHeaderOp::HeaderExists => json!([]),
                    pb::EdgeHeaderOp::HeaderEquals => json!([h.value]),
                    pb::EdgeHeaderOp::HeaderContains => json!([format!("*{}*", h.value)]),
                };
            m.insert("header".into(), json!({ h.name.clone(): values }));
        }
        Some(Kind::Ip(l)) => {
            // Behind Cloudflare the client is CF-Connecting-IP (trusted
            // only from Cloudflare's ranges, see `trusted_proxies`).
            let key = if proxied { "client_ip" } else { "remote_ip" };
            m.insert(key.into(), json!({ "ranges": l.values }));
        }
        Some(Kind::Country(l)) => {
            m.insert("header".into(), json!({ "CF-IPCountry": l.values }));
            m.insert("remote_ip".into(), json!({ "ranges": CLOUDFLARE_RANGES }));
        }
        None => {}
    }
    if c.negate {
        let mut n = Map::new();
        n.insert("not".into(), json!([Value::Object(m)]));
        return n;
    }
    m
}

/// The rule's matcher sets (any of them matches); empty = every request.
fn matchers(rule: &pb::EdgeRule, proxied: bool) -> Vec<Map<String, Value>> {
    rule.any
        .iter()
        .map(|g| {
            g.all
                .iter()
                .map(|c| condition(c, proxied))
                .fold(Map::new(), and)
        })
        .collect()
}

/// Requests a shared cache must never answer or store.
fn cacheable() -> Map<String, Value> {
    let mut m = Map::new();
    // A null list: the header is absent.
    m.insert(
        "header".into(),
        json!({ "Cookie": null, "Authorization": null }),
    );
    m
}

/// The subroute serving one site: its rules in order, then `proxy`.
/// Rules whose action is in `unsupported` are left out.
pub fn subroute(
    deployment_id: &str,
    rules: &[pb::EdgeRule],
    proxied: bool,
    proxy: &Value,
    unsupported: &BTreeSet<String>,
) -> Value {
    let mut routes = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        if unsupported.contains(action_name(rule)) {
            continue;
        }
        let Some(action) = &rule.action else { continue };
        let mut sets = matchers(rule, proxied);
        let (handle, terminal) = match action {
            Action::RateLimit(r) => {
                let key = if !r.header.is_empty() {
                    format!("{{http.request.header.{}}}", r.header)
                } else if proxied {
                    "{http.vars.client_ip}".to_owned()
                } else {
                    "{http.request.remote.host}".to_owned()
                };
                // Zones with the same name share their counters: across the
                // deployment's hostnames and across config reloads.
                let zone = format!("{deployment_id}_{i}");
                (
                    json!({
                        "handler": "rate_limit",
                        "rate_limits": { zone: {
                            "key": key,
                            "window": format!("{}s", r.window_secs),
                            "max_events": r.requests
                        } }
                    }),
                    false,
                )
            }
            Action::Block(b) => {
                let mut h = json!({ "handler": "static_response", "status_code": b.status });
                if !b.body.is_empty() {
                    h["body"] = json!(escape(&b.body));
                }
                (h, true)
            }
            Action::Allow(_) => (proxy.clone(), true),
            Action::Redirect(r) => (
                json!({
                    "handler": "static_response",
                    "status_code": r.status,
                    "headers": { "Location": [redirect_target(&r.target)] }
                }),
                true,
            ),
            Action::Headers(h) => {
                let mut response = json!({ "deferred": true });
                if !h.set.is_empty() {
                    let set: Map<String, Value> = h
                        .set
                        .iter()
                        .map(|(k, v)| (k.clone(), json!([escape(v)])))
                        .collect();
                    response["set"] = Value::Object(set);
                }
                if !h.remove.is_empty() {
                    response["delete"] = json!(h.remove);
                }
                (json!({ "handler": "headers", "response": response }), false)
            }
            Action::Cache(c) => {
                sets = if sets.is_empty() {
                    vec![cacheable()]
                } else {
                    sets.into_iter().map(|s| and(s, cacheable())).collect()
                };
                (
                    json!({ "handler": "cache", "ttl": format!("{}s", c.ttl_secs) }),
                    false,
                )
            }
        };
        let mut route = json!({ "handle": [handle] });
        if !sets.is_empty() {
            route["match"] = Value::Array(sets.into_iter().map(Value::Object).collect());
        }
        if terminal {
            route["terminal"] = json!(true);
        }
        routes.push(route);
    }
    routes.push(json!({ "handle": [proxy] }));
    json!({ "handler": "subroute", "routes": routes })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn path(op: pb::EdgePathOp, v: &str) -> pb::EdgeCondition {
        pb::EdgeCondition {
            negate: false,
            kind: Some(Kind::Path(pb::EdgePath {
                op: op as i32,
                value: v.into(),
            })),
        }
    }

    pub fn rule(groups: Vec<Vec<pb::EdgeCondition>>, action: Action) -> pb::EdgeRule {
        pb::EdgeRule {
            any: groups
                .into_iter()
                .map(|all| pb::EdgeMatchGroup { all })
                .collect(),
            action: Some(action),
        }
    }

    /// One rule of every action type, with every kind of condition.
    pub fn every_rule() -> Vec<pb::EdgeRule> {
        let list = |v: &[&str]| pb::EdgeList {
            values: v.iter().map(|s| s.to_string()).collect(),
        };
        vec![
            rule(
                vec![vec![
                    path(pb::EdgePathOp::PathPrefix, "/api"),
                    pb::EdgeCondition {
                        negate: false,
                        kind: Some(Kind::Method(list(&["POST"]))),
                    },
                ]],
                Action::RateLimit(pb::EdgeRateLimit {
                    requests: 100,
                    window_secs: 60,
                    header: String::new(),
                }),
            ),
            rule(
                vec![vec![pb::EdgeCondition {
                    negate: false,
                    kind: Some(Kind::Header(pb::EdgeHeader {
                        name: "X-Api-Key".into(),
                        op: pb::EdgeHeaderOp::HeaderExists as i32,
                        value: String::new(),
                    })),
                }]],
                Action::RateLimit(pb::EdgeRateLimit {
                    requests: 5,
                    window_secs: 1,
                    header: "X-Api-Key".into(),
                }),
            ),
            rule(
                vec![
                    vec![path(pb::EdgePathOp::PathMatches, "/admin*")],
                    vec![pb::EdgeCondition {
                        negate: false,
                        kind: Some(Kind::Country(list(&["RU", "KP"]))),
                    }],
                ],
                Action::Block(pb::EdgeBlock {
                    status: 403,
                    body: "{\"error\":\"{env.SECRET}\"}".into(),
                }),
            ),
            rule(
                vec![vec![pb::EdgeCondition {
                    negate: false,
                    kind: Some(Kind::Ip(list(&["203.0.113.0/24", "2001:db8::1"]))),
                }]],
                Action::Allow(pb::EdgeAllow {}),
            ),
            rule(
                vec![vec![path(pb::EdgePathOp::PathEquals, "/old")]],
                Action::Redirect(pb::EdgeRedirect {
                    status: 308,
                    target: "https://new.example.com{path}?{query}&x={file./etc/passwd}".into(),
                }),
            ),
            rule(
                vec![],
                Action::Headers(pb::EdgeHeaders {
                    set: [("X-Frame-Options".to_owned(), "DENY".to_owned())].into(),
                    remove: vec!["Server".into()],
                }),
            ),
            rule(
                vec![vec![
                    path(pb::EdgePathOp::PathPrefix, "/assets/"),
                    pb::EdgeCondition {
                        negate: false,
                        kind: Some(Kind::Header(pb::EdgeHeader {
                            name: "X-Preview".into(),
                            op: pb::EdgeHeaderOp::HeaderContains as i32,
                            value: "yes".into(),
                        })),
                    },
                ]],
                Action::Cache(pb::EdgeCache { ttl_secs: 300 }),
            ),
        ]
    }

    fn proxy() -> Value {
        json!({ "handler": "reverse_proxy", "upstreams": [{ "dial": "10.88.0.2:8080" }] })
    }

    #[test]
    fn every_action_renders() {
        let s = subroute("dep1", &every_rule(), false, &proxy(), &BTreeSet::new());
        let routes = s["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 8);
        // Rate limit per client IP, AND of path prefix and method.
        assert_eq!(
            routes[0],
            json!({
                "match": [{ "path": ["/api*"], "method": ["POST"] }],
                "handle": [{ "handler": "rate_limit", "rate_limits": { "dep1_0": {
                    "key": "{http.request.remote.host}", "window": "60s", "max_events": 100
                } } }]
            })
        );
        // Per header value.
        assert_eq!(
            routes[1]["handle"][0]["rate_limits"]["dep1_1"]["key"],
            "{http.request.header.X-Api-Key}"
        );
        assert_eq!(routes[1]["match"][0]["header"]["X-Api-Key"], json!([]));
        // Block: either group; the body's braces can't read anything.
        assert_eq!(
            routes[2],
            json!({
                "match": [
                    { "path": ["/admin*"] },
                    { "header": { "CF-IPCountry": ["RU", "KP"] }, "remote_ip": { "ranges": CLOUDFLARE_RANGES } }
                ],
                "handle": [{ "handler": "static_response", "status_code": 403,
                             "body": "\\{\"error\":\"\\{env.SECRET\\}\"\\}" }],
                "terminal": true
            })
        );
        // Allow: straight to the service, nothing after it runs.
        assert_eq!(
            routes[3],
            json!({
                "match": [{ "remote_ip": { "ranges": ["203.0.113.0/24", "2001:db8::1"] } }],
                "handle": [proxy()],
                "terminal": true
            })
        );
        assert_eq!(
            routes[4]["handle"][0]["headers"]["Location"][0],
            "https://new.example.com{http.request.uri.path}?{http.request.uri.query}&x=\\{file./etc/passwd\\}"
        );
        assert_eq!(routes[4]["handle"][0]["status_code"], 308);
        assert_eq!(routes[4]["match"][0]["path"], json!(["/old"]));
        // Response headers, every request.
        assert_eq!(
            routes[5],
            json!({ "handle": [{ "handler": "headers", "response": {
                "deferred": true, "set": { "X-Frame-Options": ["DENY"] }, "delete": ["Server"]
            } }] })
        );
        // Cache: never with cookies or Authorization; the header condition
        // and the credential check both use `header`, so they are ANDed by
        // double negation.
        assert_eq!(
            routes[6]["handle"][0],
            json!({ "handler": "cache", "ttl": "300s" })
        );
        assert_eq!(
            routes[6]["match"][0],
            json!({ "not": [
                { "not": [{ "path": ["/assets/*"], "header": { "X-Preview": ["*yes*"] } }] },
                { "not": [{ "header": { "Cookie": null, "Authorization": null } }] }
            ] })
        );
        assert_eq!(routes[7], json!({ "handle": [proxy()] }));
    }

    #[test]
    fn proxied_sites_use_the_cloudflare_client_ip() {
        let s = subroute("d", &every_rule(), true, &proxy(), &BTreeSet::new());
        assert_eq!(
            s["routes"][0]["handle"][0]["rate_limits"]["d_0"]["key"],
            "{http.vars.client_ip}"
        );
        assert!(s["routes"][3]["match"][0].get("client_ip").is_some());
    }

    #[test]
    fn same_matcher_twice_is_anded_by_double_negation() {
        let r = rule(
            vec![vec![
                path(pb::EdgePathOp::PathPrefix, "/a"),
                path(pb::EdgePathOp::PathMatches, "*.php"),
            ]],
            Action::Allow(pb::EdgeAllow {}),
        );
        let s = subroute("d", &[r], false, &proxy(), &BTreeSet::new());
        assert_eq!(
            s["routes"][0]["match"][0],
            json!({ "not": [{ "not": [{ "path": ["/a*"] }] }, { "not": [{ "path": ["*.php"] }] }] })
        );
        // A negated condition is a `not` of its own.
        let mut c = path(pb::EdgePathOp::PathEquals, "/health");
        c.negate = true;
        let r = rule(
            vec![vec![c]],
            Action::Block(pb::EdgeBlock {
                status: 404,
                body: String::new(),
            }),
        );
        let s = subroute("d", &[r], false, &proxy(), &BTreeSet::new());
        assert_eq!(
            s["routes"][0]["match"][0],
            json!({ "not": [{ "path": ["/health"] }] })
        );
    }

    #[test]
    fn unsupported_actions_are_left_out() {
        let unsupported: BTreeSet<String> = ["rate_limit".to_owned(), "cache".to_owned()].into();
        let s = subroute("d", &every_rule(), false, &proxy(), &unsupported);
        let handlers: Vec<&str> = s["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["handle"][0]["handler"].as_str().unwrap())
            .collect();
        assert_eq!(
            handlers,
            [
                "static_response",
                "reverse_proxy",
                "static_response",
                "headers",
                "reverse_proxy"
            ]
        );
    }

    #[test]
    fn caddy_unknown_module_errors() {
        // As Caddy 2.11.1 without the plugin reports it.
        let e = "Caddy rejected the config (400): {\"error\":\"loading new config: loading http app module: provision http: server modula: setting up route handlers: route 0: loading handler modules: position 0: loading module 'subroute': provision http.handlers.subroute: setting up subroutes: route 0: loading handler modules: position 0: loading module 'rate_limit': unknown module: http.handlers.rate_limit\"}";
        assert_eq!(missing_action(e), Some("rate_limit"));
        assert_eq!(
            missing_action("loading module 'cache': unknown module: http.handlers.cache"),
            Some("cache")
        );
        assert_eq!(missing_action("unknown module: http.handlers.nope"), None);
        assert_eq!(missing_action("some other error"), None);
    }
}
