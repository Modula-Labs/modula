//! What `server create` tells you while it works. People get step lines on
//! stderr (`✓ Booting Ubuntu  24s`, a live teal spinner with elapsed time on
//! terminals); `--json` gets one JSON object per line on stdout, an
//! [`Event`] each: a `step` event whenever a step starts, finishes, is
//! skipped or fails, plus milestones (`server_created`, `dns_waiting`, …),
//! ending with a `result` event.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::output::Output;

/// One line of `--json` output. `event` names the kind; the other fields
/// are camelCase.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A step changed: `running`, then `done` (with its time), `skipped` or
    /// `failed`.
    #[serde(rename_all = "camelCase")]
    Step {
        name: String,
        state: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_secs: Option<u64>,
    },
    /// Everything was checked; nothing has been created yet.
    Plan {
        provider: String,
        location: String,
        #[serde(rename = "type")]
        server_type: String,
        name: String,
        domain: String,
        #[serde(rename = "monthlyCents")]
        monthly_cents: i64,
        monthly: String,
    },
    #[serde(rename_all = "camelCase")]
    FirewallCreated { id: i64, name: String, reused: bool },
    #[serde(rename_all = "camelCase")]
    SshKeyUploaded { id: i64, name: String },
    #[serde(rename_all = "camelCase")]
    ServerCreated { id: i64, name: String, ip: String },
    #[serde(rename_all = "camelCase")]
    ServerRunning { id: i64, elapsed_secs: u64 },
    #[serde(rename_all = "camelCase")]
    DnsRecordCreated {
        name: String,
        ip: String,
        zone: String,
    },
    /// Add this record (unless Cloudflare does it); polled until it resolves.
    #[serde(rename_all = "camelCase")]
    DnsWaiting {
        record_type: String,
        name: String,
        value: String,
        current: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    DnsReady {
        name: String,
        ip: String,
        elapsed_secs: u64,
    },
    /// The installer reported a failure (step, message, its hint).
    #[serde(rename_all = "camelCase")]
    InstallFailed {
        step: String,
        message: String,
        hint: String,
    },
    #[serde(rename_all = "camelCase")]
    SetupComplete { email: String },
    #[serde(rename_all = "camelCase")]
    LoggedIn { url: String, email: String },
    #[serde(rename_all = "camelCase")]
    Ready {
        url: String,
        ip: String,
        server_id: i64,
        elapsed_secs: u64,
    },
    #[serde(rename_all = "camelCase")]
    Error {
        message: String,
        hint: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Cleanup {
        server_deleted: bool,
        firewall_deleted: bool,
        dns_record_deleted: bool,
    },
}

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Progress {
    pub out: Output,
    /// Live lines (spinner, `\r`) on stderr.
    live: bool,
    started: Instant,
    step: Option<(String, Instant)>,
    frame: usize,
}

impl Progress {
    pub fn new(out: Output) -> Self {
        let live = !out.json
            && !out.is_captured()
            && std::io::stderr().is_terminal()
            && std::env::var("TERM").map_or(true, |t| t != "dumb");
        Self {
            out,
            live,
            started: Instant::now(),
            step: None,
            frame: 0,
        }
    }

    pub fn json(&self) -> bool {
        self.out.json
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// With `--json`: the event as one line on stdout.
    pub fn event(&self, e: &Event) {
        if self.out.json
            && let Ok(line) = serde_json::to_string(e)
        {
            self.out.line(line);
        }
    }

    fn err(&self, text: &str) {
        if !self.out.json && !self.out.is_captured() {
            let mut e = std::io::stderr().lock();
            let _ = e.write_all(text.as_bytes());
            let _ = e.flush();
        }
    }

    /// A step begins: `→ label…`.
    pub fn start(&mut self, label: &str) {
        self.step = Some((label.to_owned(), Instant::now()));
        self.event(&Event::Step {
            name: label.to_owned(),
            state: "running",
            elapsed_secs: None,
        });
        if self.live {
            self.err(&format!("  {} {label}…", self.out.dim("→")));
        } else {
            self.err(&format!("  → {label}…\n"));
        }
    }

    /// The running step is still going: redraw its line with the time so
    /// far and what we're waiting on (terminals only).
    pub fn tick(&mut self, detail: Option<&str>) {
        let Some((label, t)) = &self.step else { return };
        if !self.live {
            return;
        }
        self.frame = (self.frame + 1) % FRAMES.len();
        let detail = detail.map(|d| format!(" · {d}")).unwrap_or_default();
        let line = format!(
            "\r\x1b[K  {} {label}  {}",
            self.out.accent(FRAMES[self.frame]),
            self.out.dim(&format!("{}{detail}", duration(t.elapsed())))
        );
        self.err(&line);
    }

    /// The step finished: `✓ label  12s` (`label` may say how it went; the
    /// event keeps the name it started with).
    pub fn done(&mut self, label: &str) {
        let (name, took) = match self.step.take() {
            Some((name, t)) => (name, t.elapsed()),
            None => (label.to_owned(), Duration::ZERO),
        };
        self.event(&Event::Step {
            name,
            state: "done",
            elapsed_secs: Some(took.as_secs()),
        });
        let took = duration(took);
        let clear = if self.live { "\r\x1b[K" } else { "" };
        self.err(&format!(
            "{clear}  {} {label:<54} {}\n",
            self.out.accent("✓"),
            self.out.dim(&took)
        ));
    }

    /// The step failed: `✗ label`.
    pub fn failed(&mut self) {
        if let Some((label, t)) = self.step.take() {
            self.event(&Event::Step {
                name: label.clone(),
                state: "failed",
                elapsed_secs: Some(t.elapsed().as_secs()),
            });
            let clear = if self.live { "\r\x1b[K" } else { "" };
            self.err(&format!("{clear}  ✗ {label}\n"));
        }
    }

    /// A step that didn't need doing: `– label  skipped`.
    pub fn skipped(&mut self, label: &str) {
        self.event(&Event::Step {
            name: label.to_owned(),
            state: "skipped",
            elapsed_secs: None,
        });
        let clear = if self.live && self.step.is_some() {
            "\r\x1b[K"
        } else {
            ""
        };
        self.err(&format!(
            "{clear}  {} {}\n",
            self.out.dim("–"),
            self.out.dim(&format!("{label:<54} skipped"))
        ));
    }

    /// Something that finished alongside the running step (e.g. DNS while
    /// installing): its own `✓` line; the spinner redraws below it.
    pub fn aside(&mut self, label: &str) {
        let clear = if self.live && self.step.is_some() {
            "\r\x1b[K"
        } else {
            ""
        };
        self.err(&format!("{clear}  {} {label}\n", self.out.accent("✓")));
    }

    /// An indented hint under the current step (people only).
    pub fn note(&mut self, text: &str) {
        let clear = if self.live && self.step.is_some() {
            "\r\x1b[K"
        } else {
            ""
        };
        self.err(&format!("{clear}    {text}\n"));
    }
}

/// `42s`, `3m 05s`, `1h 02m`.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_one_json_object_per_line() {
        let (out, buf) = Output::capture(true);
        let p = Progress::new(out);
        p.event(&Event::ServerCreated {
            id: 7,
            name: "modula-main".into(),
            ip: "203.0.113.10".into(),
        });
        p.event(&Event::DnsWaiting {
            record_type: "A".into(),
            name: "deploy.example.com".into(),
            value: "203.0.113.10".into(),
            current: vec![],
        });
        p.event(&Event::Plan {
            provider: "hetzner".into(),
            location: "fsn1".into(),
            server_type: "cx33".into(),
            name: "modula-main".into(),
            domain: "deploy.example.com".into(),
            monthly_cents: 749,
            monthly: "€7.49".into(),
        });
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            serde_json::json!({"event": "server_created", "id": 7, "name": "modula-main", "ip": "203.0.113.10"})
        );
        assert_eq!(lines[1]["event"], "dns_waiting");
        assert_eq!(lines[1]["recordType"], "A");
        assert_eq!(lines[2]["type"], "cx33");
        assert_eq!(lines[2]["monthlyCents"], 749);
    }

    #[test]
    fn people_get_no_json() {
        let (out, buf) = Output::capture(false);
        let mut p = Progress::new(out);
        p.start("Creating the server");
        p.tick(Some("booting"));
        p.done("Created the server");
        p.skipped("Firewall");
        p.aside("deploy.example.com points at 203.0.113.10");
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn steps_become_events() {
        let (out, buf) = Output::capture(true);
        let mut p = Progress::new(out);
        p.start("Booting Ubuntu");
        p.done("Server running");
        p.skipped("Firewall");
        p.start("Installing Modula");
        p.failed();
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            serde_json::json!({"event": "step", "name": "Booting Ubuntu", "state": "running"})
        );
        assert_eq!(
            lines[1]["name"], "Booting Ubuntu",
            "the name it started with"
        );
        assert_eq!(lines[1]["state"], "done");
        assert_eq!(lines[1]["elapsedSecs"], 0);
        assert_eq!(lines[2]["state"], "skipped");
        assert_eq!(lines[4]["state"], "failed");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_secs(42)), "42s");
        assert_eq!(duration(Duration::from_secs(185)), "3m 05s");
        assert_eq!(duration(Duration::from_secs(3720)), "1h 02m");
    }
}
