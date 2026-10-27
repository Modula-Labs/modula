//! A service's edge rules (`ServiceConfig::edge_rules`): checked here, sent
//! to the nodes serving its domains with each route (`Route.rules`) and
//! rendered into Caddy there. They are a staged setting like the rest of
//! the config: a deployment carries the rules it was deployed with, so a
//! rollback brings its rules back too.

use std::collections::BTreeMap;
use std::net::IpAddr;

use modula_proto::node::v1 as pb;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

pub const MAX_RULES: usize = 10;
const MAX_GROUPS: usize = 5;
const MAX_CONDITIONS: usize = 5;
const MAX_LIST: usize = 50;
const MAX_NAME: usize = 60;
const MAX_BODY: usize = 4096;
const MAX_HEADERS: usize = 20;
const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeRule {
    #[serde(default)]
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Any of the groups, all conditions of a group; empty = every request.
    #[serde(default)]
    pub when: Vec<Vec<Condition>>,
    pub then: Action,
}

fn yes() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PathOp {
    /// `*` wildcards (Caddy's path matcher).
    Matches,
    Prefix,
    Equals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HeaderOp {
    Exists,
    Equals,
    Contains,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "field",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Condition {
    Path {
        op: PathOp,
        value: String,
        #[serde(default, skip_serializing_if = "is_false")]
        negate: bool,
    },
    Method {
        values: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        negate: bool,
    },
    Header {
        name: String,
        op: HeaderOp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        negate: bool,
    },
    /// Client IP in one of these CIDRs or addresses.
    Ip {
        cidrs: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        negate: bool,
    },
    /// ISO 3166 country codes from Cloudflare (proxied domains only).
    Country {
        values: Vec<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        negate: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RateKey {
    Ip,
    Header,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Action {
    /// At most `requests` per `window_secs` per key, per server; more get 429.
    RateLimit {
        requests: u32,
        window_secs: u32,
        key: RateKey,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<String>,
    },
    Block {
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
    /// Stop evaluating rules; the request goes to the service.
    Allow,
    Redirect {
        status: u16,
        /// May contain `{host}`, `{path}` and `{query}`.
        target: String,
    },
    /// Response headers.
    Headers {
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        set: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        remove: Vec<String>,
    },
    /// Cache responses for `ttl_secs` (shorter if the response says so).
    /// Requests with cookies or Authorization are never cached.
    Cache { ttl_secs: u32 },
}

fn bad(i: usize, msg: impl std::fmt::Display) -> AppError {
    AppError::bad_request(format!("edge rule {}: {msg}", i + 1))
}

/// An HTTP field name (RFC 9110 token).
fn header_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 100
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Printable text without braces (matchers would read them as placeholders).
fn match_value_ok(v: &str, max: usize) -> bool {
    !v.is_empty()
        && v.len() <= max
        && v.bytes()
            .all(|b| (0x20..0x7f).contains(&b) && b != b'{' && b != b'}')
}

/// A header value we send: no control characters.
fn header_value_ok(v: &str) -> bool {
    v.len() <= 1024 && !v.chars().any(|c| c.is_control())
}

fn cidr_ok(s: &str) -> bool {
    let (ip, prefix) = match s.split_once('/') {
        Some((ip, p)) => (ip, Some(p)),
        None => (s, None),
    };
    let Ok(ip) = ip.parse::<IpAddr>() else {
        return false;
    };
    let max = if ip.is_ipv4() { 32 } else { 128 };
    prefix.is_none_or(|p| p.parse::<u8>().is_ok_and(|p| p <= max) && !p.starts_with('+'))
}

fn list_ok(v: &[String]) -> bool {
    !v.is_empty() && v.len() <= MAX_LIST
}

impl Condition {
    fn validate(&self, i: usize) -> AppResult<()> {
        match self {
            Condition::Path { op, value, .. } => {
                let starts_ok =
                    value.starts_with('/') || (*op == PathOp::Matches && value.starts_with('*'));
                if !starts_ok {
                    return Err(bad(i, "a path starts with /"));
                }
                if !match_value_ok(value, 512) || value.contains(' ') {
                    return Err(bad(i, "the path is too long or has spaces or braces"));
                }
                if *op != PathOp::Matches && value.contains('*') {
                    return Err(bad(i, "* only works with \"matches\""));
                }
            }
            Condition::Method { values, .. } => {
                if !list_ok(values) || values.iter().any(|m| !METHODS.contains(&m.as_str())) {
                    return Err(bad(
                        i,
                        format!("methods must be some of {}", METHODS.join(", ")),
                    ));
                }
            }
            Condition::Header {
                name, op, value, ..
            } => {
                if !header_name_ok(name) {
                    return Err(bad(i, "invalid header name"));
                }
                match (op, value) {
                    (HeaderOp::Exists, _) => {}
                    (_, Some(v)) if match_value_ok(v, 512) => {}
                    _ => {
                        return Err(bad(
                            i,
                            "the header value is required (printable, no braces)",
                        ));
                    }
                }
            }
            Condition::Ip { cidrs, .. } => {
                if !list_ok(cidrs) || !cidrs.iter().all(|c| cidr_ok(c)) {
                    return Err(bad(i, "IPs must be addresses or CIDRs like 203.0.113.0/24"));
                }
            }
            Condition::Country { values, .. } => {
                let ok = values
                    .iter()
                    .all(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()));
                if !list_ok(values) || !ok {
                    return Err(bad(i, "countries are two-letter codes like US or DE"));
                }
            }
        }
        Ok(())
    }

    fn to_pb(&self) -> pb::EdgeCondition {
        use pb::edge_condition::Kind;
        let list = |v: &[String]| pb::EdgeList { values: v.to_vec() };
        let (negate, kind) = match self {
            Condition::Path { op, value, negate } => (
                *negate,
                Kind::Path(pb::EdgePath {
                    op: match op {
                        PathOp::Matches => pb::EdgePathOp::PathMatches,
                        PathOp::Prefix => pb::EdgePathOp::PathPrefix,
                        PathOp::Equals => pb::EdgePathOp::PathEquals,
                    } as i32,
                    value: value.clone(),
                }),
            ),
            Condition::Method { values, negate } => (*negate, Kind::Method(list(values))),
            Condition::Header {
                name,
                op,
                value,
                negate,
            } => (
                *negate,
                Kind::Header(pb::EdgeHeader {
                    name: name.clone(),
                    op: match op {
                        HeaderOp::Exists => pb::EdgeHeaderOp::HeaderExists,
                        HeaderOp::Equals => pb::EdgeHeaderOp::HeaderEquals,
                        HeaderOp::Contains => pb::EdgeHeaderOp::HeaderContains,
                    } as i32,
                    value: value.clone().unwrap_or_default(),
                }),
            ),
            Condition::Ip { cidrs, negate } => (*negate, Kind::Ip(list(cidrs))),
            Condition::Country { values, negate } => (*negate, Kind::Country(list(values))),
        };
        pb::EdgeCondition {
            negate,
            kind: Some(kind),
        }
    }
}

impl EdgeRule {
    fn validate(&self, i: usize) -> AppResult<()> {
        if self.name.chars().count() > MAX_NAME || self.name.chars().any(char::is_control) {
            return Err(bad(i, "the name is at most 60 characters"));
        }
        if self.when.len() > MAX_GROUPS
            || self
                .when
                .iter()
                .any(|g| g.is_empty() || g.len() > MAX_CONDITIONS)
        {
            return Err(bad(i, "at most 5 alternatives of 1–5 conditions each"));
        }
        for c in self.when.iter().flatten() {
            c.validate(i)?;
        }
        match &self.then {
            Action::RateLimit {
                requests,
                window_secs,
                key,
                header,
            } => {
                if !(1..=1_000_000).contains(requests) || !(1..=3600).contains(window_secs) {
                    return Err(bad(
                        i,
                        "rate limits are 1–1000000 requests per 1–3600 seconds",
                    ));
                }
                match (key, header) {
                    (RateKey::Ip, None) => {}
                    (RateKey::Header, Some(h)) if header_name_ok(h) => {}
                    (RateKey::Header, _) => {
                        return Err(bad(i, "a valid header name is required"));
                    }
                    (RateKey::Ip, Some(_)) => {
                        return Err(bad(i, "a header is only used with key \"header\""));
                    }
                }
            }
            Action::Block { status, body } => {
                if !(400..=599).contains(status) {
                    return Err(bad(i, "block status must be 400–599"));
                }
                if body
                    .as_ref()
                    .is_some_and(|b| b.len() > MAX_BODY || b.contains('\0'))
                {
                    return Err(bad(i, "the response body is at most 4 KB"));
                }
            }
            Action::Allow => {}
            Action::Redirect { status, target } => {
                if ![301, 302, 307, 308].contains(status) {
                    return Err(bad(i, "redirect status must be 301, 302, 307 or 308"));
                }
                let scheme_ok = target.starts_with('/')
                    || target.starts_with("https://")
                    || target.starts_with("http://");
                if !scheme_ok
                    || target.len() > 2048
                    || target.bytes().any(|b| !(0x21..0x7f).contains(&b))
                {
                    return Err(bad(
                        i,
                        "the redirect target is a /path or an http(s):// URL without spaces",
                    ));
                }
            }
            Action::Headers { set, remove } => {
                if set.is_empty() && remove.is_empty() {
                    return Err(bad(i, "set or remove at least one header"));
                }
                if set.len() + remove.len() > MAX_HEADERS
                    || !set
                        .iter()
                        .all(|(k, v)| header_name_ok(k) && header_value_ok(v))
                    || !remove.iter().all(|k| header_name_ok(k))
                {
                    return Err(bad(i, "invalid header name or value"));
                }
            }
            Action::Cache { ttl_secs } => {
                if !(1..=604_800).contains(ttl_secs) {
                    return Err(bad(i, "cache TTL must be 1 second – 7 days"));
                }
            }
        }
        Ok(())
    }

    pub fn to_pb(&self) -> pb::EdgeRule {
        use pb::edge_rule::Action as A;
        let action = match &self.then {
            Action::RateLimit {
                requests,
                window_secs,
                header,
                ..
            } => A::RateLimit(pb::EdgeRateLimit {
                requests: *requests,
                window_secs: *window_secs,
                header: header.clone().unwrap_or_default(),
            }),
            Action::Block { status, body } => A::Block(pb::EdgeBlock {
                status: u32::from(*status),
                body: body.clone().unwrap_or_default(),
            }),
            Action::Allow => A::Allow(pb::EdgeAllow {}),
            Action::Redirect { status, target } => A::Redirect(pb::EdgeRedirect {
                status: u32::from(*status),
                target: target.clone(),
            }),
            Action::Headers { set, remove } => A::Headers(pb::EdgeHeaders {
                set: set.clone().into_iter().collect(),
                remove: remove.clone(),
            }),
            Action::Cache { ttl_secs } => A::Cache(pb::EdgeCache {
                ttl_secs: *ttl_secs,
            }),
        };
        pb::EdgeRule {
            any: self
                .when
                .iter()
                .map(|g| pb::EdgeMatchGroup {
                    all: g.iter().map(Condition::to_pb).collect(),
                })
                .collect(),
            action: Some(action),
        }
    }
}

pub fn validate(rules: &[EdgeRule]) -> AppResult<()> {
    if rules.len() > MAX_RULES {
        return Err(AppError::bad_request(format!(
            "at most {MAX_RULES} edge rules"
        )));
    }
    for (i, r) in rules.iter().enumerate() {
        r.validate(i)?;
    }
    Ok(())
}

/// The enabled rules, as nodes get them.
pub fn to_pb(rules: &[EdgeRule]) -> Vec<pb::EdgeRule> {
    rules
        .iter()
        .filter(|r| r.enabled)
        .map(EdgeRule::to_pb)
        .collect()
}

/// A node's name for a rule action (`StateReport.edge_unsupported`) as
/// the dashboard names it.
pub fn action_label(node_name: &str) -> Option<&'static str> {
    match node_name {
        "rate_limit" => Some("rateLimit"),
        "cache" => Some("cache"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(v: serde_json::Value) -> Vec<EdgeRule> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn every_rule_type_parses_validates_and_converts() {
        let r = rules(json!([
            { "name": "API", "enabled": true,
              "when": [[{ "field": "path", "op": "prefix", "value": "/api" },
                        { "field": "method", "values": ["POST"] }]],
              "then": { "type": "rateLimit", "requests": 100, "windowSecs": 60, "key": "ip" } },
            { "name": "", "enabled": true,
              "when": [[{ "field": "header", "name": "X-Api-Key", "op": "exists" }]],
              "then": { "type": "rateLimit", "requests": 5, "windowSecs": 1, "key": "header", "header": "X-Api-Key" } },
            { "name": "Admin", "enabled": true,
              "when": [[{ "field": "path", "op": "matches", "value": "/admin*" }],
                       [{ "field": "country", "values": ["RU"] }]],
              "then": { "type": "block", "status": 403, "body": "{\"error\":1}" } },
            { "name": "Office", "enabled": true,
              "when": [[{ "field": "ip", "cidrs": ["203.0.113.0/24", "2001:db8::1"], "negate": true }]],
              "then": { "type": "allow" } },
            { "name": "Moved", "enabled": true,
              "when": [[{ "field": "path", "op": "equals", "value": "/old" }]],
              "then": { "type": "redirect", "status": 308, "target": "https://new.example.com{path}?{query}" } },
            { "name": "Security", "enabled": true, "when": [],
              "then": { "type": "headers", "set": { "X-Frame-Options": "DENY" }, "remove": ["Server"] } },
            { "name": "Assets", "enabled": false,
              "when": [[{ "field": "path", "op": "prefix", "value": "/assets/" }]],
              "then": { "type": "cache", "ttlSecs": 300 } }
        ]));
        validate(&r).unwrap();
        let sent = to_pb(&r);
        // The disabled rule stays in the config but isn't sent.
        assert_eq!(sent.len(), 6);
        assert_eq!(
            sent[0].action,
            Some(pb::edge_rule::Action::RateLimit(pb::EdgeRateLimit {
                requests: 100,
                window_secs: 60,
                header: String::new()
            }))
        );
        assert_eq!(sent[2].any.len(), 2);
        assert!(sent[3].any[0].all[0].negate);
        // Round trip through JSON keeps the shape the dashboard sends.
        let back: Vec<EdgeRule> =
            serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn invalid_rules_are_refused() {
        let bad_rules = [
            json!({ "when": [[{ "field": "path", "op": "prefix", "value": "admin" }]], "then": { "type": "allow" } }),
            json!({ "when": [[{ "field": "path", "op": "equals", "value": "/{env.X}" }]], "then": { "type": "allow" } }),
            json!({ "when": [[{ "field": "path", "op": "prefix", "value": "/a*" }]], "then": { "type": "allow" } }),
            json!({ "when": [[{ "field": "method", "values": ["BREW"] }]], "then": { "type": "allow" } }),
            json!({ "when": [[{ "field": "header", "name": "X Bad", "op": "exists" }]], "then": { "type": "allow" } }),
            json!({ "when": [[{ "field": "header", "name": "X-A", "op": "equals" }]], "then": { "type": "allow" } }),
            json!({ "when": [[{ "field": "ip", "cidrs": ["10.0.0.0/33"] }]], "then": { "type": "allow" } }),
            json!({ "when": [[{ "field": "ip", "cidrs": [] }]], "then": { "type": "allow" } }),
            json!({ "when": [[{ "field": "country", "values": ["usa"] }]], "then": { "type": "allow" } }),
            json!({ "when": [[]], "then": { "type": "allow" } }),
            json!({ "then": { "type": "rateLimit", "requests": 0, "windowSecs": 60, "key": "ip" } }),
            json!({ "then": { "type": "rateLimit", "requests": 10, "windowSecs": 7200, "key": "ip" } }),
            json!({ "then": { "type": "rateLimit", "requests": 10, "windowSecs": 60, "key": "header" } }),
            json!({ "then": { "type": "block", "status": 200 } }),
            json!({ "then": { "type": "block", "status": 403, "body": "x".repeat(5000) } }),
            json!({ "then": { "type": "redirect", "status": 303, "target": "/a" } }),
            json!({ "then": { "type": "redirect", "status": 301, "target": "javascript:alert(1)" } }),
            json!({ "then": { "type": "redirect", "status": 301, "target": "/a b" } }),
            json!({ "then": { "type": "headers" } }),
            json!({ "then": { "type": "headers", "set": { "X-A": "line\nbreak" } } }),
            json!({ "then": { "type": "cache", "ttlSecs": 0 } }),
            json!({ "name": "x".repeat(61), "then": { "type": "allow" } }),
        ];
        for r in bad_rules {
            let parsed: EdgeRule = serde_json::from_value(r.clone()).unwrap();
            assert!(validate(&[parsed]).is_err(), "{r}");
        }
        // Unknown fields and actions don't even parse.
        assert!(
            serde_json::from_value::<EdgeRule>(json!({ "then": { "type": "teapot" } })).is_err()
        );
        assert!(
            serde_json::from_value::<EdgeRule>(json!({ "then": { "type": "allow" }, "x": 1 }))
                .is_err()
        );
        let many: Vec<EdgeRule> = (0..11)
            .map(|_| serde_json::from_value(json!({ "then": { "type": "allow" } })).unwrap())
            .collect();
        assert!(validate(&many).is_err());
        assert!(validate(&many[..10]).is_ok());
    }
}
