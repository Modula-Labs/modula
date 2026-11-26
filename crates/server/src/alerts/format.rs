//! What each channel kind receives: Slack Block Kit, a Discord embed,
//! Modula's own JSON (signed by the sender, see `send`), or an email
//! (plain text plus simple HTML).

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
        Phase::Test | Phase::Report => a.title.clone(),
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
        Phase::Report => 0x0F766E,
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
        Phase::Report => "report.weekly",
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

/// An email's subject, plain text and HTML.
#[derive(Debug, Clone, PartialEq)]
pub struct Mail {
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// Escapes text for HTML element content and attribute values.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Status colors: red firing, green resolved, grey for a test, teal reports.
fn mail_color(p: Phase) -> &'static str {
    match p {
        Phase::Fired => "#E5484D",
        Phase::Resolved => "#30A46C",
        Phase::Test => "#8B8D98",
        Phase::Report => "#0F766E",
    }
}

const FONT: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif";
const INK: &str = "#1c1d1f";
const MUTED: &str = "#6b6f76";
const LINE: &str = "#eceef0";
const ACCENT: &str = "#0f766e";

/// One email's content, laid out by [`page`]. Everything in it is plain text
/// except `body`, which is HTML the caller escaped.
pub struct Page<'a> {
    /// The inbox preview line.
    pub preheader: &'a str,
    /// A small status label above the title, and its color.
    pub badge: (&'a str, &'a str),
    pub title: &'a str,
    pub body: String,
    /// The main button: label and URL.
    pub primary: Option<(&'a str, &'a str)>,
    /// Smaller links under the button.
    pub secondary: Vec<(&'a str, String)>,
    /// "What to try" steps.
    pub steps: Vec<&'a str>,
    /// Small print at the bottom.
    pub footer: String,
    /// The dashboard's origin, for the wordmark and settings links.
    pub origin: &'a str,
}

/// The shared email layout: wordmark, status, title, body, actions, steps,
/// footer, on a plain white page that reads well in every mail client.
pub fn page(p: &Page) -> String {
    let e = html_escape;
    let (badge, color) = p.badge;
    let button = p
        .primary
        .map(|(label, url)| {
            format!(
                "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"margin:28px 0 8px\"><tr>\
<td style=\"background:{ACCENT};border-radius:8px\"><a href=\"{}\" style=\"display:inline-block;padding:13px 26px;color:#ffffff;font-size:15px;font-weight:600;text-decoration:none\">{}</a></td></tr></table>",
                e(url),
                e(label)
            )
        })
        .unwrap_or_default();
    let secondary = if p.secondary.is_empty() {
        String::new()
    } else {
        let links: Vec<String> = p
            .secondary
            .iter()
            .map(|(label, url)| {
                format!(
                    "<a href=\"{}\" style=\"color:{ACCENT};text-decoration:none;font-weight:500\">{} →</a>",
                    e(url),
                    e(label)
                )
            })
            .collect();
        format!(
            "<p style=\"margin:12px 0 0;font-size:14px;line-height:22px\">{}</p>",
            links.join("&nbsp;&nbsp;·&nbsp;&nbsp;")
        )
    };
    let steps = if p.steps.is_empty() {
        String::new()
    } else {
        let items: String = p
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                format!(
                    "<tr><td valign=\"top\" style=\"padding:0 12px 12px 0;width:22px\">\
<div style=\"width:22px;height:22px;border-radius:11px;background:#e6f4f1;color:{ACCENT};font-size:12px;font-weight:600;line-height:22px;text-align:center\">{}</div></td>\
<td style=\"padding:1px 0 12px;font-size:15px;line-height:22px;color:{INK}\">{}</td></tr>",
                    i + 1,
                    e(s)
                )
            })
            .collect();
        format!(
            "<h2 style=\"margin:36px 0 14px;font-size:16px;font-weight:600;color:{INK}\">What to try</h2>\
<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" style=\"border-collapse:collapse\">{items}</table>"
        )
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<meta name=\"color-scheme\" content=\"light\"><title>{title}</title></head>\
<body style=\"margin:0;padding:0;background:#ffffff;font-family:{FONT};color:{INK}\">\
<div style=\"display:none;max-height:0;overflow:hidden;opacity:0\">{preheader}</div>\
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background:#ffffff\"><tr><td align=\"center\" style=\"padding:40px 20px\">\
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"max-width:560px\">\
<tr><td style=\"padding:0 0 40px\"><a href=\"{origin}\" style=\"text-decoration:none;color:{INK}\">\
<span style=\"display:inline-block;width:26px;height:26px;border-radius:7px;background:{ACCENT};color:#ffffff;font-size:16px;font-weight:700;line-height:26px;text-align:center;vertical-align:middle\">M</span>\
<span style=\"font-size:19px;font-weight:700;letter-spacing:-0.2px;vertical-align:middle;margin-left:8px\">Modula</span></a></td></tr>\
<tr><td>\
<span style=\"display:inline-block;padding:3px 10px;border-radius:999px;background:{color}1a;color:{color};font-size:12px;font-weight:600;letter-spacing:0.3px;text-transform:uppercase\">{badge}</span>\
<h1 style=\"margin:14px 0 16px;font-size:26px;line-height:32px;font-weight:700;letter-spacing:-0.3px;color:{INK}\">{title}</h1>\
{body}{button}{secondary}{steps}\
<p style=\"margin:44px 0 0;font-size:13px;line-height:20px;color:{MUTED}\">{footer}</p>\
</td></tr></table></td></tr></table></body></html>",
        title = e(p.title),
        preheader = e(p.preheader),
        origin = e(p.origin),
        badge = e(badge),
        body = p.body,
        footer = p.footer,
    )
}

/// A paragraph of body text (`html` already escaped).
pub fn paragraph(html: &str) -> String {
    format!("<p style=\"margin:0 0 16px;font-size:16px;line-height:26px;color:#3b3d42\">{html}</p>")
}

/// Bold body text (escaped).
pub fn bold_html(s: &str) -> String {
    bold(s)
}

fn bold(s: &str) -> String {
    format!("<b style=\"color:{INK}\">{}</b>", html_escape(s))
}

/// The dashboard origin of a link (`https://host[:port]`).
pub fn origin_of(link: &str) -> String {
    url::Url::parse(link)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|_| link.to_owned())
}

/// `link` with its `tab` query parameter set to `tab` (and any deployment
/// dropped), for links into other tabs of the same service.
fn with_tab(link: &str, tab: &str) -> String {
    let Ok(mut u) = url::Url::parse(link) else {
        return link.to_owned();
    };
    let pairs: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| k != "tab" && k != "deployment")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("tab", tab);
    u.into()
}

/// "**api** in **anton** (**production**)", from what the alert has.
fn where_html(a: &Alert) -> String {
    let mut s = match &a.service {
        Some(svc) => format!("your service {}", bold(svc)),
        None => "your service".to_owned(),
    };
    if let Some(p) = &a.project {
        s.push_str(&format!(" in {}", bold(p)));
    }
    if let Some(env) = &a.environment {
        s.push_str(&format!(" ({})", bold(env)));
    }
    s
}

/// The intro sentence, main button label, extra links and steps per kind.
#[allow(clippy::type_complexity)]
fn guidance(a: &Alert) -> (String, &'static str, Vec<(&'static str, String)>, Vec<&'static str>) {
    use super::Kind::*;
    let at = where_html(a);
    let server = a
        .server
        .as_deref()
        .map(bold)
        .unwrap_or_else(|| "A server".into());
    let resolved = a.phase == Phase::Resolved;
    let l = &a.link;
    match (a.phase, a.kind) {
        (Phase::Test, _) => (
            "This is a test from Modula. If you can read this, the email channel works and alerts will arrive here.".into(),
            "Open Modula",
            vec![],
            vec![],
        ),
        (_, Some(DeploymentFailed)) => (
            format!("The latest deployment of {at} failed after its build, so the previous deployment is still serving."),
            "View deployment",
            vec![("Variables", with_tab(l, "variables")), ("Settings", with_tab(l, "settings"))],
            vec![
                "Open the deployment's logs and look for the first error.",
                "Check the health check path and port in the service's Settings.",
                "Make sure every variable the app needs is set.",
                "Fix and redeploy, or roll back from the Deployments tab.",
            ],
        ),
        (_, Some(BuildFailed)) => (
            format!("The build for {at} failed, so nothing new was deployed."),
            "View build logs",
            vec![("Settings", with_tab(l, "settings"))],
            vec![
                "Read the build log from the bottom up for the failing step.",
                "Run the same build locally (install, then build) to reproduce it.",
                "Check the builder, root directory and build command in Settings.",
                "Push a fix: a new commit builds and deploys automatically.",
            ],
        ),
        (_, Some(DeploymentCrashed)) => (
            format!("{} crashed{}.", capitalize(&at), if resolved { "" } else { " and is being restarted" }),
            "View logs",
            vec![("Metrics", with_tab(l, "metrics")), ("Settings", with_tab(l, "settings"))],
            vec![
                "Check the logs just before the crash for the error.",
                "Exit code 137 means it ran out of memory: raise the memory limit in Settings.",
                "If a recent change caused it, roll back from the Deployments tab.",
            ],
        ),
        (_, Some(DeploymentDegraded)) => (
            format!("A replica of {at} crashed. The other replicas are still serving traffic."),
            "View logs",
            vec![("Metrics", with_tab(l, "metrics"))],
            vec![
                "Check the crashed replica's logs for the error.",
                "Look at memory and CPU in Metrics around the time of the crash.",
            ],
        ),
        (_, Some(BackupFailed)) => (
            format!("A backup of {at} failed. Your earlier backups are untouched."),
            "View backups",
            vec![("Service settings", with_tab(l, "settings"))],
            vec![
                "Check the error below and the backup destination's credentials.",
                "Make sure the destination bucket or disk has space.",
                "Run a backup now from the Backups tab to confirm the fix.",
            ],
        ),
        (_, Some(BackupCompleted)) => (
            format!("A backup of {at} finished successfully."),
            "View backups",
            vec![],
            vec![],
        ),
        (Phase::Resolved, Some(Cpu | Memory)) => (
            format!("Usage of {at} is back under the threshold."),
            "View metrics",
            vec![],
            vec![],
        ),
        (_, Some(Cpu)) => (
            format!("{} has been using a lot of CPU.", capitalize(&at)),
            "View metrics",
            vec![("Settings", with_tab(l, "settings"))],
            vec![
                "Check Metrics for when it started and whether traffic rose with it.",
                "Raise the CPU limit in Settings, or add replicas to share the load.",
                "Look for a busy loop or a slow query if traffic didn't change.",
            ],
        ),
        (_, Some(Memory)) => (
            format!("{} is close to its memory limit. If it reaches it, it will be restarted.", capitalize(&at)),
            "View metrics",
            vec![("Settings", with_tab(l, "settings"))],
            vec![
                "Check Metrics for steady growth, which usually means a leak.",
                "Raise the memory limit in Settings.",
            ],
        ),
        (Phase::Resolved, Some(Disk)) => (
            format!("The disk on {server} has space again."),
            "View server",
            vec![],
            vec![],
        ),
        (_, Some(Disk)) => (
            format!("The disk on {server} is almost full. Deployments, databases and backups on it can fail when it fills up."),
            "View server",
            vec![("All servers", format!("{}/servers", origin_of(l)))],
            vec![
                "Remove unused images: docker image prune -a on the server.",
                "Check which volumes and backups use the most space.",
                "Resize the disk or move services to another server.",
            ],
        ),
        (Phase::Resolved, Some(NodeOffline)) => (
            format!("{server} is reporting to Modula again."),
            "View server",
            vec![],
            vec![],
        ),
        (_, Some(NodeOffline)) => (
            format!("{server} has stopped reporting to Modula. Services on it may be down."),
            "View server",
            vec![("All servers", format!("{}/servers", origin_of(l)))],
            vec![
                "Check that the machine is running in your provider's console.",
                "On the server, run systemctl status modula-node and read its logs.",
                "Make sure it can still reach the main server (firewall, network).",
            ],
        ),
        _ => (paragraph_safe(&a.title), "Open in Modula", vec![], vec![]),
    }
}

fn paragraph_safe(s: &str) -> String {
    html_escape(s)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Error output and other details, in a quiet monospace box; "Key: value"
/// lines become a two-column table instead.
fn reason_html(a: &Alert) -> String {
    if a.reason.is_empty() {
        return String::new();
    }
    let kv: Option<Vec<(&str, &str)>> = a
        .reason
        .lines()
        .map(|l| l.split_once(": ").filter(|(k, _)| k.len() <= 20))
        .collect();
    match kv {
        Some(rows) if a.kind == Some(super::Kind::BackupCompleted) => table(&rows),
        _ => {
            let label = match a.phase {
                Phase::Fired => "Details",
                _ => "Now",
            };
            format!(
                "<p style=\"margin:20px 0 6px;font-size:12px;font-weight:600;letter-spacing:0.3px;text-transform:uppercase;color:{MUTED}\">{label}</p>\
<div style=\"padding:14px 16px;background:#f6f7f8;border-radius:8px;font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:13px;line-height:20px;color:{INK};white-space:pre-wrap;word-break:break-word\">{}</div>",
                html_escape(&a.reason)
            )
        }
    }
}

/// A quiet two-column table of (label, value).
pub fn table(rows: &[(&str, &str)]) -> String {
    let cells: String = rows
        .iter()
        .map(|(k, v)| {
            format!(
                "<tr><td style=\"padding:10px 16px 10px 0;border-bottom:1px solid {LINE};font-size:14px;color:{MUTED};white-space:nowrap\">{}</td>\
<td style=\"padding:10px 0;border-bottom:1px solid {LINE};font-size:14px;color:{INK}\">{}</td></tr>",
                html_escape(k),
                html_escape(v)
            )
        })
        .collect();
    format!(
        "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"border-collapse:collapse;margin:8px 0 0\">{cells}</table>"
    )
}

pub fn email(a: &Alert) -> Mail {
    email_in(a, None)
}

/// The workspace's notification settings in the dashboard.
pub fn settings_link(origin: &str, workspace: Option<modula_id::Id>) -> String {
    match workspace {
        Some(w) => format!("{origin}/settings?workspace={w}#notifications"),
        None => format!("{origin}/settings#notifications"),
    }
}

/// [`email`], with links that open `workspace` in the dashboard.
pub fn email_in(a: &Alert, workspace: Option<modula_id::Id>) -> Mail {
    // Subjects are one line; Cloudflare caps them at 998 characters.
    let head = headline(a).replace(['\r', '\n'], " ");
    let subject = truncate(&format!("[Modula] {head}"), 200);
    let f = fields(a);
    let when = timestamp(a);
    let (intro, action, secondary, mut steps) = guidance(a);
    if a.phase == Phase::Resolved {
        steps.clear();
    }

    let mut text = format!("{head}\n\n");
    if !a.reason.is_empty() {
        text.push_str(&a.reason);
        text.push_str("\n\n");
    }
    for (k, v) in &f {
        text.push_str(&format!("{k}: {v}\n"));
    }
    text.push_str(&format!(
        "Time: {when}\nRule: {}\n\nOpen in Modula: {}\n",
        a.rule_name, a.link
    ));
    if !steps.is_empty() {
        text.push_str("\nWhat to try:\n");
        for (i, s) in steps.iter().enumerate() {
            text.push_str(&format!("{}. {s}\n", i + 1));
        }
    }

    let origin = origin_of(&a.link);
    let badge = match (a.phase, a.kind) {
        (Phase::Test, _) => "Test",
        (Phase::Resolved, _) => "Resolved",
        (_, Some(super::Kind::BackupCompleted)) => "Backup completed",
        (Phase::Report, _) => "Report",
        _ => "Alert",
    };
    let color = match (a.phase, a.kind) {
        (Phase::Fired, Some(super::Kind::BackupCompleted)) => mail_color(Phase::Resolved),
        (p, _) => mail_color(p),
    };
    let mut body = paragraph(&intro);
    body.push_str(&reason_html(a));
    let when_human = a
        .at
        .format(time::macros::format_description!(
            "[day padding:none] [month repr:short] [year], [hour]:[minute] UTC"
        ))
        .unwrap_or(when.clone());
    let footer = format!(
        "{} · rule <b>{}</b><br>You get this email because this address is a notification channel in Modula. \
<a href=\"{}\" style=\"color:{MUTED}\">Manage notifications</a>",
        html_escape(&when_human),
        html_escape(&a.rule_name),
        html_escape(&settings_link(&origin, workspace)),
    );
    let html = page(&Page {
        preheader: &truncate(&a.reason.replace('\n', " · "), 140),
        badge: (badge, color),
        title: &head,
        body,
        primary: Some((action, &a.link)),
        secondary,
        steps,
        footer,
        origin: &origin,
    });
    Mail {
        subject,
        text,
        html,
    }
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
    #[test]
    fn email_text_and_html() {
        let m = email(&alert(Phase::Fired));
        assert_eq!(m.subject, "[Modula] web failed to deploy");
        assert!(
            m.text
                .starts_with("web failed to deploy\n\nhealth check <timeout> & retry\n\n")
        );
        assert!(
            m.text
                .contains("Project: shop\nEnvironment: production\nService: web\n")
        );
        assert!(m.text.contains("Time: 2026-10-24T12:00:00Z"));
        assert!(
            m.text
                .contains("Open in Modula: https://cloud.example.com/projects/p?env=e\n")
        );
        assert!(m.text.contains("\nWhat to try:\n1. "));
        // Everything user-controlled is escaped in the HTML.
        assert!(m.html.contains("health check &lt;timeout&gt; &amp; retry"));
        assert!(!m.html.contains("<timeout>"));
        assert!(
            m.html
                .contains("href=\"https://cloud.example.com/projects/p?env=e\"")
        );
        assert!(m.html.contains("#E5484D"));
        let r = email(&alert(Phase::Resolved));
        assert_eq!(r.subject, "[Modula] Resolved: web failed to deploy");
        assert!(r.html.contains("#30A46C"));
        // No header injection through the title.
        let mut evil = alert(Phase::Fired);
        evil.title = "a\r\nBcc: x@y.z".into();
        assert!(!email(&evil).subject.contains('\n'));
        assert_eq!(html_escape("\"'<&>"), "&quot;&#39;&lt;&amp;&gt;");
    }

    #[test]
    fn email_links_open_real_pages() {
        let mut a = alert(Phase::Fired);
        a.link = "https://cloud.example.com/projects/p?env=e&service=s&tab=deployments&deployment=d".into();
        let m = email_in(&a, Some("wwwwwwwwwwwwwwww".parse().unwrap()));
        // The main button opens the deployment; the others its service's tabs.
        assert!(m.html.contains("href=\"https://cloud.example.com/projects/p?env=e&amp;service=s&amp;tab=deployments&amp;deployment=d\""));
        assert!(m.html.contains("href=\"https://cloud.example.com/projects/p?env=e&amp;service=s&amp;tab=variables\""));
        assert!(m.html.contains("href=\"https://cloud.example.com/settings?workspace=wwwwwwwwwwwwwwww#notifications\""));
        assert!(m.html.contains(">View deployment<"));
        assert!(m.html.contains("What to try"));
        // Server alerts: the server's page, and all servers.
        let mut n = alert(Phase::Fired);
        n.kind = Some(Kind::NodeOffline);
        n.link = "https://cloud.example.com/servers/n1".into();
        let m = email(&n);
        assert!(m.html.contains(">View server<"));
        assert!(m.html.contains("href=\"https://cloud.example.com/servers\""));
        // Resolved alerts don't ask for anything.
        assert!(!email(&alert(Phase::Resolved)).html.contains("What to try"));
    }
}
