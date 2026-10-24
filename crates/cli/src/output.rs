//! What commands print: aligned tables and terse lines for people, JSON for
//! scripts and agents (`--json`). Results go to stdout (or a buffer in
//! tests); hints and progress go to stderr so `--json` output stays parseable.

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// A mistake in how the command was invoked: exit code 2, like clap's own.
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Fail with a usage error (exit code 2).
pub fn usage(msg: impl Into<String>) -> anyhow::Error {
    UsageError(msg.into()).into()
}

#[derive(Clone)]
enum Sink {
    Stdout,
    Buffer(Arc<Mutex<Vec<u8>>>),
}

#[derive(Clone)]
pub struct Output {
    pub json: bool,
    color: bool,
    /// The terminal takes 24-bit colors (`COLORTERM=truecolor`).
    truecolor: bool,
    sink: Sink,
}

impl Output {
    /// Stdout; colors only on a terminal without `NO_COLOR`.
    pub fn stdout(json: bool) -> Self {
        let color = !json
            && std::io::stdout().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        let truecolor = matches!(
            std::env::var("COLORTERM").as_deref(),
            Ok("truecolor" | "24bit")
        );
        Self {
            json,
            color,
            truecolor,
            sink: Sink::Stdout,
        }
    }

    /// Everything written goes to the returned buffer (for tests).
    pub fn capture(json: bool) -> (Self, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let out = Self {
            json,
            color: false,
            truecolor: false,
            sink: Sink::Buffer(buf.clone()),
        };
        (out, buf)
    }

    pub fn is_captured(&self) -> bool {
        matches!(self.sink, Sink::Buffer(_))
    }

    fn write(&self, text: &str) {
        match &self.sink {
            Sink::Stdout => {
                let mut out = std::io::stdout().lock();
                // A closed pipe (`| head`) isn't worth an error.
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
            }
            Sink::Buffer(b) => {
                if let Ok(mut b) = b.lock() {
                    b.extend_from_slice(text.as_bytes());
                }
            }
        }
    }

    /// One line of the result.
    pub fn line(&self, text: impl AsRef<str>) {
        self.write(&format!("{}\n", text.as_ref()));
    }

    /// The result as pretty JSON.
    pub fn json<T: Serialize + ?Sized>(&self, value: &T) -> anyhow::Result<()> {
        self.line(serde_json::to_string_pretty(value)?);
        Ok(())
    }

    /// The JSON value with `--json`, else the human rendering.
    pub fn either<T: Serialize + ?Sized>(
        &self,
        value: &T,
        human: impl FnOnce(&Self),
    ) -> anyhow::Result<()> {
        if self.json {
            self.json(value)
        } else {
            human(self);
            Ok(())
        }
    }

    pub fn table(&self, table: &Table) {
        self.write(&table.render(self.color));
    }

    /// Progress and hints for people, on stderr (never part of the result).
    pub fn note(&self, text: impl AsRef<str>) {
        if !self.is_captured() {
            eprintln!("{}", text.as_ref());
        }
    }

    /// `text` in Modula's teal when colors are on.
    pub fn accent(&self, text: &str) -> String {
        match (self.color, self.truecolor) {
            (false, _) => text.to_owned(),
            (true, true) => format!("\x1b[38;2;45;212;191m{text}\x1b[0m"),
            (true, false) => format!("\x1b[38;5;43m{text}\x1b[0m"),
        }
    }

    /// `text` dimmed when colors are on.
    pub fn dim(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    /// `status`, colored by how good it is when colors are on.
    pub fn status(&self, status: &str) -> String {
        paint(status, self.color)
    }
}

fn paint(status: &str, color: bool) -> String {
    if !color {
        return status.to_owned();
    }
    let code = match status {
        "active" | "online" | "verified" | "SUCCESS" | "succeeded" => "32",
        "failed" | "crashed" | "error" | "timed_out" => "31",
        "queued" | "building" | "deploying" | "pending" | "running" => "33",
        "removed" | "cancelled" | "offline" | "skipped" => "2",
        _ => return status.to_owned(),
    };
    format!("\x1b[{code}m{status}\x1b[0m")
}

/// Columns aligned with two spaces; the header in capitals.
#[derive(Debug, Default)]
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    /// The column whose values are statuses (colored on terminals).
    status_column: Option<usize>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_uppercase()).collect(),
            ..Self::default()
        }
    }

    /// Headers exactly as given (e.g. SQL column names).
    pub fn verbatim(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| (*h).to_owned()).collect(),
            ..Self::default()
        }
    }

    pub fn status_column(mut self, column: usize) -> Self {
        self.status_column = Some(column);
        self
    }

    pub fn row<I, S>(&mut self, cells: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rows.push(cells.into_iter().map(Into::into).collect());
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn render(&self, color: bool) -> String {
        let cols = self.headers.len();
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(cols) {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let mut out = String::new();
        let mut line = |cells: &[String], header: bool| {
            let mut text = String::new();
            for (i, width) in widths.iter().enumerate() {
                let cell = cells.get(i).map(String::as_str).unwrap_or("");
                let last = i + 1 == cols;
                let shown = if !header && self.status_column == Some(i) {
                    paint(cell, color)
                } else {
                    cell.to_owned()
                };
                text.push_str(&shown);
                if !last {
                    let pad = width - cell.chars().count() + 2;
                    text.extend(std::iter::repeat_n(' ', pad));
                }
            }
            out.push_str(text.trim_end());
            out.push('\n');
        };
        line(&self.headers, true);
        for row in &self.rows {
            line(row, false);
        }
        out
    }
}

/// `2026-10-23T12:34:56Z` → `2026-10-23 12:34` (UTC, as the server sends it).
pub fn short_time(rfc3339: &str) -> String {
    match rfc3339.get(..16) {
        Some(t) if rfc3339.len() > 16 => t.replacen('T', " ", 1),
        _ => rfc3339.to_owned(),
    }
}

/// At most `max` characters, the first line only, with `…` when cut.
pub fn truncate(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or("");
    if first.chars().count() <= max && first.len() == text.trim_end().len() {
        return first.to_owned();
    }
    let cut: String = first.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// `-` for nothing.
pub fn or_dash(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.is_empty() => v.to_owned(),
        _ => "-".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_align_and_trim() {
        let mut t = Table::new(&["service", "status", "url"]).status_column(1);
        t.row(["api", "active", "https://api.example.com"]);
        t.row(["postgres", "failed", ""]);
        assert_eq!(
            t.render(false),
            "SERVICE   STATUS  URL\n\
             api       active  https://api.example.com\n\
             postgres  failed\n"
        );
        // Colors don't change the alignment.
        let colored = t.render(true);
        assert!(
            colored.contains("\x1b[32mactive\x1b[0m  https://"),
            "{colored}"
        );
    }

    #[test]
    fn json_goes_to_the_sink() {
        let (out, buf) = Output::capture(true);
        out.either(&serde_json::json!({ "ok": true }), |_| unreachable!())
            .unwrap();
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn small_formatters() {
        assert_eq!(short_time("2026-10-23T12:34:56.123Z"), "2026-10-23 12:34");
        assert_eq!(short_time("soon"), "soon");
        assert_eq!(truncate("fix: a bug\n\nlong body", 40), "fix: a bug…");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(or_dash(None), "-");
        assert_eq!(or_dash(Some("x")), "x");
    }
}
