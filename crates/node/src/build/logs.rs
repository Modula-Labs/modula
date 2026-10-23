//! Build output, one file per deployment: `<ts_ms> <o|e> <text>` per line.
//! Streamed to the server on request (like container logs) and kept after
//! the build so failed builds can be read later.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use modula_id::Id;

use crate::runtime::LogLine;

const MAX_LINE: usize = 8 * 1024;
/// A runaway build stops logging (but keeps building) past this size.
const MAX_FILE: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct BuildLogs {
    dir: PathBuf,
}

impl BuildLogs {
    pub fn new(dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, deployment: &str) -> Option<PathBuf> {
        deployment
            .parse::<Id>()
            .ok()
            .map(|id| self.dir.join(format!("{id}.log")))
    }

    /// A fresh log for a deployment's build (truncates a previous attempt).
    pub fn create(&self, deployment: &str) -> anyhow::Result<BuildLog> {
        let path = self
            .path(deployment)
            .ok_or_else(|| anyhow::anyhow!("bad deployment id"))?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        let _ = std::fs::remove_file(done_marker(&path));
        Ok(BuildLog {
            inner: Arc::new(Mutex::new(Inner { file, written: 0 })),
            path,
        })
    }

    /// The deployment's log for more output after its build (or a new one
    /// for an image deployment), e.g. its pre-deploy command.
    pub fn append(&self, deployment: &str) -> anyhow::Result<BuildLog> {
        let path = self
            .path(deployment)
            .ok_or_else(|| anyhow::anyhow!("bad deployment id"))?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        let _ = std::fs::remove_file(done_marker(&path));
        Ok(BuildLog {
            inner: Arc::new(Mutex::new(Inner { file, written })),
            path,
        })
    }

    /// The last `tail` lines, then (with `follow`) new lines until the build ends.
    pub fn read(&self, deployment: &str, tail: u32, follow: bool) -> Option<Reader> {
        let path = self.path(deployment)?;
        path.exists().then_some(Reader {
            path,
            tail,
            follow,
            offset: None,
        })
    }

    /// Delete logs older than `age` (called periodically).
    pub fn prune(&self, age: Duration) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for e in entries.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|d| d > age));
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn done_marker(path: &Path) -> PathBuf {
    path.with_extension("done")
}

struct Inner {
    file: File,
    written: u64,
}

#[derive(Clone)]
pub struct BuildLog {
    inner: Arc<Mutex<Inner>>,
    path: PathBuf,
}

impl BuildLog {
    pub fn line(&self, stderr: bool, text: &str) {
        let ts = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
        let mut text: String = text.chars().filter(|c| *c != '\r').collect();
        if text.len() > MAX_LINE {
            let cut = (0..=MAX_LINE)
                .rev()
                .find(|i| text.is_char_boundary(*i))
                .unwrap_or(0);
            text.truncate(cut);
        }
        let mut inner = self.inner.lock().expect("build log lock");
        if inner.written > MAX_FILE {
            return;
        }
        let rec = format!("{ts} {} {text}\n", if stderr { 'e' } else { 'o' });
        if inner.file.write_all(rec.as_bytes()).is_ok() {
            inner.written += rec.len() as u64;
        }
    }

    /// Marks the log complete so followers stop waiting.
    pub fn finish(&self) {
        let _ = File::create(done_marker(&self.path));
    }
}

pub struct Reader {
    path: PathBuf,
    tail: u32,
    follow: bool,
    offset: Option<u64>,
}

impl Reader {
    /// The next batch of lines; `None` once there is nothing more to wait for.
    pub async fn next_batch(&mut self) -> Option<Vec<LogLine>> {
        loop {
            let done = done_marker(&self.path).exists();
            let (lines, offset) = read_from(&self.path, self.offset, self.tail).ok()?;
            let first = self.offset.is_none();
            self.offset = Some(offset);
            if !lines.is_empty() || first {
                return Some(lines);
            }
            if !self.follow || done {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Lines after `offset`, or the last `tail` lines when starting.
fn read_from(path: &Path, offset: Option<u64>, tail: u32) -> std::io::Result<(Vec<LogLine>, u64)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = offset.unwrap_or(0).min(len);
    f.seek(SeekFrom::Start(start))?;
    let mut lines = Vec::new();
    let mut consumed = start;
    for raw in BufReader::new(f).lines() {
        let raw = raw?;
        consumed += raw.len() as u64 + 1;
        if consumed > len {
            break; // a partial last line; read it next time
        }
        lines.push(parse(&raw));
    }
    if offset.is_none() && lines.len() > tail as usize {
        lines.drain(..lines.len() - tail as usize);
    }
    Ok((lines, consumed.min(len)))
}

fn parse(raw: &str) -> LogLine {
    let mut parts = raw.splitn(3, ' ');
    let ts_unix_ms = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    let stderr = parts.next() == Some("e");
    LogLine {
        ts_unix_ms,
        stderr,
        text: parts.next().unwrap_or_default().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_tail_follow() {
        let dir = std::env::temp_dir().join(format!("modula-buildlogs-{}", Id::new()));
        let logs = BuildLogs::new(&dir).unwrap();
        let id = Id::new().to_string();
        let log = logs.create(&id).unwrap();
        for i in 0..5 {
            log.line(i == 4, &format!("line {i}"));
        }
        let mut r = logs.read(&id, 2, true).unwrap();
        let first = r.next_batch().await.unwrap();
        assert_eq!(
            first.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["line 3", "line 4"]
        );
        assert!(first[1].stderr);
        log.line(false, "line 5");
        assert_eq!(r.next_batch().await.unwrap()[0].text, "line 5");
        log.finish();
        assert!(r.next_batch().await.is_none());
        assert!(logs.read("not-an-id", 10, false).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
