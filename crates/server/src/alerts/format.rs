//! What each channel kind receives: Slack Block Kit, a Discord embed, or
//! Modula's own JSON (signed by the sender, see `send`).

use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use super::{Alert, Phase};

fn timestamp(a: &Alert) -> String {
    a.at.format(&Rfc3339).unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// (label, value) of the alert's context, the ones it has.
fn fields(a: &Alert) -> Vec<(&'static str, &str)> {
    [
        ("Project", a.project.as_deref()),
        ("Environment", a.environment.as_deref()),
        ("Service", a.service.as_deref()),
        ("Server", a.server.as_deref()),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|v| (k, v)))
    .collect()
}

fn headline(a: &Alert) -> String {
    match a.phase {
        Phase::Fired => a.title.clone(),
        Phase::Resolved => format!("Resolved: {}", a.title),
        Phase::Test => a.title.clone(),
    }
}

/// Slack's mrkdwn treats these three as control characters.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn slack(a: &Alert) -> Value {
    let head = truncate(&headline(a), 150);
    let mut blocks = vec![json!({
        "type": "header",
        "text": { "type": "plain_text", "text": head, "emoji": false },
    })];
    if !a.reason.is_empty() {
        blocks.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": truncate(&slack_escape(&a.reason), 2900) },
        }));
    }
    let f = fields(a);
    if !f.is_empty() {
        blocks.push(json!({
            "type": "section",
            "fields": f.iter().map(|(k, v)| json!({
                "type": "mrkdwn",
                "text": format!("*{k}*\n{}", truncate(&slack_escape(v), 500)),
            })).collect::<Vec<_>>(),
        }));
    }
    blocks.push(json!({
        "type": "actions",
        "elements": [{
            "type": "button",
            "text": { "type": "plain_text", "text": "Open in Modula" },
            "url": a.link,
        }],
    }));
    blocks.push(json!({
        "type": "context",
        "elements": [{
            "type": "mrkdwn",
            "text": format!(
                "<!date^{}^{{date_short_pretty}} {{time}}|{}> · {}",
                a.at.unix_timestamp(),
                timestamp(a),
                slack_escape(&a.rule_name)
            ),
        }],
    }));
    json!({ "text": head, "blocks": blocks })
}

/// Red while firing, green when resolved, grey for a test.
fn discord_color(p: Phase) -> u32 {
    match p {
        Phase::Fired => 0xE5484D,
        Phase::Resolved => 0x30A46C,
        Phase::Test => 0x8B8D98,
    }
}

pub fn discord(a: &Alert) -> Value {
    json!({
        "username": "Modula",
        "allowed_mentions": { "parse": [] },
        "embeds": [{
            "title": truncate(&headline(a), 256),
            "description": truncate(&a.reason, 4000),
            "url": a.link,
            "color": discord_color(a.phase),
            "fields": fields(a).iter().map(|(k, v)| json!({
                "name": k, "value": truncate(v, 1024), "inline": true,
            })).collect::<Vec<_>>(),
            "footer": { "text": truncate(&a.rule_name, 2048) },
            "timestamp": timestamp(a),
        }],
    })
}

/// `X-Modula-Event` of a generic webhook delivery.
pub fn event_name(p: Phase) -> &'static str {
    match p {
        Phase::Fired => "alert.fired",
        Phase::Resolved => "alert.resolved",
        Phase::Test => "test",
    }
}

pub fn webhook(a: &Alert) -> Value {
    json!({
        "type": event_name(a.phase),
        "timestamp": timestamp(a),
        "rule": {
            "id": a.rule_id,
            "name": a.rule_name,
            "kind": a.kind.map(|k| k.as_str()),
        },
        "subject": a.subject,
        "title": a.title,
        "reason": a.reason,
        "project": a.project,
        "environment": a.environment,
        "service": a.service,
        "server": a.server,
        "url": a.link,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::Kind;

    fn alert(phase: Phase) -> Alert {
        Alert {
            phase,
            rule_id: Some("aaaaaaaaaaaaaaaa".parse().unwrap()),
            rule_name: "Prod deploys".into(),
            kind: Some(Kind::DeploymentFailed),
            subject: "instance:x".into(),
            title: "web failed to deploy".into(),
            reason: "health check <timeout> & retry".into(),
            project: Some("shop".into()),
            environment: Some("production".into()),
            service: Some("web".into()),
            server: None,
            link: "https://cloud.example.com/projects/p?env=e".into(),
            at: time::macros::datetime!(2026-10-24 12:00 UTC),
        }
    }

    #[test]
    fn slack_blocks() {
        let v = slack(&alert(Phase::Fired));
        assert_eq!(v["text"], "web failed to deploy");
        let blocks = v["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(
            blocks[1]["text"]["text"],
            "health check &lt;timeout&gt; &amp; retry"
        );
        let fields = blocks[2]["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0]["text"], "*Project*\nshop");
        assert_eq!(
            blocks[3]["elements"][0]["url"],
            "https://cloud.example.com/projects/p?env=e"
        );
        let ctx = blocks[4]["elements"][0]["text"].as_str().unwrap();
        assert!(ctx.starts_with("<!date^1792843200^"), "{ctx}");
        assert!(ctx.contains("2026-10-24T12:00:00Z"));
        let resolved = slack(&alert(Phase::Resolved));
        assert_eq!(resolved["text"], "Resolved: web failed to deploy");
    }

    #[test]
    fn discord_embed() {
        let v = discord(&alert(Phase::Fired));
        let e = &v["embeds"][0];
        assert_eq!(e["title"], "web failed to deploy");
        assert_eq!(e["description"], "health check <timeout> & retry");
        assert_eq!(e["url"], "https://cloud.example.com/projects/p?env=e");
        assert_eq!(e["color"], 0xE5484D);
        assert_eq!(e["fields"][1]["name"], "Environment");
        assert_eq!(e["fields"][1]["value"], "production");
        assert_eq!(e["timestamp"], "2026-10-24T12:00:00Z");
        assert_eq!(v["allowed_mentions"]["parse"], json!([]));
        assert_eq!(
            discord(&alert(Phase::Resolved))["embeds"][0]["color"],
            0x30A46C
        );
        // Discord's limits.
        let mut long = alert(Phase::Fired);
        long.title = "x".repeat(400);
        let t = discord(&long)["embeds"][0]["title"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(t.chars().count(), 256);
    }

    #[test]
    fn webhook_json() {
        let v = webhook(&alert(Phase::Resolved));
        assert_eq!(v["type"], "alert.resolved");
        assert_eq!(v["rule"]["kind"], "deployment_failed");
        assert_eq!(v["rule"]["name"], "Prod deploys");
        assert_eq!(v["service"], "web");
        assert_eq!(v["server"], Value::Null);
        assert_eq!(v["url"], "https://cloud.example.com/projects/p?env=e");
        assert_eq!(event_name(Phase::Test), "test");
    }
}
