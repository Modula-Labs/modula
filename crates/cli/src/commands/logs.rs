//! `logs`: a deployment's runtime or build logs, from the API's
//! Server-Sent Events stream (`log`, `error`, `end`).

use futures::StreamExt;
use serde::Deserialize;

use crate::api::endpoints::LogOptions;
use crate::cli::LogsArgs;
use crate::context::Ctx;

/// One parsed event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Splits a byte stream into events; tolerates chunks that end anywhere.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() || !self.event.is_empty() {
                    out.push(SseEvent {
                        event: if self.event.is_empty() {
                            "message".into()
                        } else {
                            std::mem::take(&mut self.event)
                        },
                        data: self.data.join("\n"),
                    });
                    self.data.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // keep-alive comment
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = value.to_owned(),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
        out
    }
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct LogLine {
    #[serde(default)]
    ts: Option<i64>,
    #[serde(default)]
    stderr: bool,
    #[serde(default)]
    text: String,
}

/// Print a deployment's log stream until it ends. With `--json`, one JSON
/// object per line (`{"ts","stderr","text"}`).
pub async fn print_stream(ctx: &Ctx, deployment: &str, opts: LogOptions) -> anyhow::Result<()> {
    let res = ctx.api()?.logs(deployment, opts).await?;
    let mut body = res.bytes_stream();
    let mut parser = SseParser::default();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("the log stream broke: {e}"))?;
        for ev in parser.push(&chunk) {
            match ev.event.as_str() {
                "log" => {
                    let Ok(line) = serde_json::from_str::<LogLine>(&ev.data) else {
                        continue;
                    };
                    if ctx.out.json {
                        ctx.out.line(serde_json::to_string(&line)?);
                    } else {
                        ctx.out.line(&line.text);
                    }
                }
                "error" => {
                    let msg = serde_json::from_str::<serde_json::Value>(&ev.data)
                        .ok()
                        .and_then(|v| v["message"].as_str().map(str::to_owned))
                        .unwrap_or(ev.data);
                    anyhow::bail!("{msg}");
                }
                "end" => return Ok(()),
                _ => {}
            }
        }
    }
    Ok(())
}

/// `30s`, `15m`, `1h`, `2d` (a bare number is minutes) in milliseconds.
fn parse_since(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "m"),
    };
    let n: i64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("--since is a duration like 30s, 15m, 1h or 2d"))?;
    let ms = match unit {
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => anyhow::bail!("--since is a duration like 30s, 15m, 1h or 2d"),
    };
    Ok(n.saturating_mul(ms).max(1000))
}

/// `2026-11-18 09:41:07` (UTC) from Unix milliseconds.
fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `logs --http`: the environment's HTTP request logs, oldest first. With
/// `--json`, one request per line as the API returns it.
async fn run_http(ctx: &Ctx, args: LogsArgs) -> anyhow::Result<i32> {
    let project = ctx.project().await?;
    let environment = ctx.environment(&project)?.id.clone();
    // Only the default scope needs a service picked.
    let service = if args.all || args.deployment.is_some() {
        None
    } else {
        Some(ctx.service(&project)?.id.clone())
    };
    let limit = args.lines.clamp(1, 5000);
    let mut since = now_ms() - parse_since(&args.since)?;
    let mut first = true;
    loop {
        // Built in its own block: the serializer isn't Send.
        let query = {
            let mut q = url::form_urlencoded::Serializer::new(String::new());
            q.append_pair("since", &since.to_string());
            q.append_pair("limit", &limit.to_string());
            if let Some(d) = &args.deployment {
                q.append_pair("deployment", d);
            } else if let Some(s) = &service {
                q.append_pair("service", s);
            }
            if let Some(s) = &args.status {
                q.append_pair("status", s);
            }
            if let Some(m) = &args.method {
                q.append_pair("method", &m.to_ascii_uppercase());
            }
            if let Some(p) = &args.path {
                q.append_pair("path", p);
            }
            q.finish()
        };
        let found = ctx.api()?.http_logs(&environment, &query).await?;
        let requests = found["requests"].as_array().cloned().unwrap_or_default();
        if first && found["truncated"].as_bool() == Some(true) && !ctx.out.json {
            eprintln!("(showing the newest {limit}; narrow --since or raise -n)");
        }
        for r in &requests {
            if let Some(ts) = r["t"].as_i64() {
                since = since.max(ts + 1);
            }
            if ctx.out.json {
                ctx.out.line(serde_json::to_string(r)?);
                continue;
            }
            let svc = if args.all {
                format!(" [{}]", r["serviceName"].as_str().unwrap_or("?"))
            } else {
                String::new()
            };
            ctx.out.line(format!(
                "{}{svc} {} {:<6} {}{} {:.0}ms {}B {}",
                utc(r["t"].as_i64().unwrap_or(0)),
                r["status"].as_i64().unwrap_or(0),
                r["method"].as_str().unwrap_or("?"),
                r["host"].as_str().unwrap_or(""),
                r["path"].as_str().unwrap_or(""),
                r["durationMs"].as_f64().unwrap_or(0.0),
                r["bytes"].as_i64().unwrap_or(0),
                r["clientIp"].as_str().unwrap_or(""),
            ));
        }
        if !args.follow {
            if first && requests.is_empty() && !ctx.out.json {
                eprintln!("no requests in the last {}", args.since);
            }
            return Ok(0);
        }
        first = false;
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            _ = tokio::signal::ctrl_c() => return Ok(0),
        }
    }
}

pub async fn run(ctx: &Ctx, args: LogsArgs) -> anyhow::Result<i32> {
    if args.http {
        return run_http(ctx, args).await;
    }
    let deployment = match args.deployment {
        Some(d) => d,
        None => {
            let t = ctx.target().await?;
            let deployments = ctx.api()?.deployments(&t.instance).await?;
            let latest = deployments
                .first()
                .ok_or_else(|| anyhow::anyhow!("{} has no deployments yet", t.label()))?;
            latest.id.clone()
        }
    };
    let opts = LogOptions {
        tail: args.lines.min(5000),
        follow: args.follow,
        build: args.build,
    };
    let stream = print_stream(ctx, &deployment, opts);
    if args.follow {
        tokio::select! {
            r = stream => r?,
            _ = tokio::signal::ctrl_c() => {}
        }
    } else {
        stream.await?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_and_utc() {
        assert_eq!(parse_since("15m").unwrap(), 900_000);
        assert_eq!(parse_since("2d").unwrap(), 172_800_000);
        assert_eq!(parse_since("5").unwrap(), 300_000);
        assert!(parse_since("1w").is_err());
        assert_eq!(utc(1_794_994_867_000), "2026-11-18 09:41:07");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00");
    }

    #[test]
    fn events_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.push(b"event: log\ndata: {\"te").is_empty());
        let evs = p.push(b"xt\":\"hi\"}\n\n:keep-alive\n\nevent: end\r\ndata: {}\r\n\r\n");
        assert_eq!(
            evs,
            [
                SseEvent {
                    event: "log".into(),
                    data: "{\"text\":\"hi\"}".into()
                },
                SseEvent {
                    event: "end".into(),
                    data: "{}".into()
                },
            ]
        );
        let evs = p.push(b"data: a\ndata: b\n\n");
        assert_eq!(evs[0].event, "message");
        assert_eq!(evs[0].data, "a\nb");
    }
}
