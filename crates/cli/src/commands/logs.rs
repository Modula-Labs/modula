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

pub async fn run(ctx: &Ctx, args: LogsArgs) -> anyhow::Result<i32> {
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
