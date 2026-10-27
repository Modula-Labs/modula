//! Ships every running workload's output to the server, which keeps it for a
//! week: logs outlive the container (each deploy replaces it) and can be
//! searched across an environment.
//!
//! One follower per running container appends to a shared, bounded buffer;
//! every [`EVERY`] the buffer goes to the outbox as one `LogBatch` (the outbox
//! survives reconnects; a full one keeps the lines for the next round).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use modula_proto::node::v1 as pb;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

pub const EVERY: Duration = Duration::from_secs(2);
/// Lines held while the server can't take them; newer ones are dropped.
const BUFFER: usize = 50_000;
/// Longer lines are cut (a stray binary dump shouldn't fill the database).
const MAX_LINE: usize = 8 * 1024;
const PER_BATCH: usize = 5_000;

#[derive(Default)]
struct Shared {
    lines: Vec<pb::ShippedLine>,
    dropped: u32,
    /// Per container: the newest line shipped (unix ms), where a restarted
    /// follower resumes.
    last: HashMap<String, i64>,
}

pub async fn run(
    runtime: Arc<dyn Runtime>,
    outbox: mpsc::Sender<pb::NodeMessage>,
    stop: CancellationToken,
) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let mut followers: HashMap<String, JoinHandle<()>> = HashMap::new();
    // Containers already running when the node starts resume from now: their
    // earlier output was shipped by the previous process.
    let started = now_ms();
    let mut first = true;
    let mut tick = tokio::time::interval(EVERY);
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tick.tick() => {}
        }
        if let Ok(containers) = runtime.containers().await {
            let running: Vec<_> = containers
                .into_iter()
                .filter(|c| c.running && !c.deployment_id.is_empty())
                .collect();
            followers.retain(|id, h| {
                let keep = !h.is_finished() && running.iter().any(|c| &c.id == id);
                if !keep {
                    h.abort();
                }
                keep
            });
            {
                let mut s = shared.lock().expect("logship lock");
                s.last.retain(|id, _| running.iter().any(|c| &c.id == id));
            }
            for c in running {
                if followers.contains_key(&c.id) {
                    continue;
                }
                let from = {
                    let s = shared.lock().expect("logship lock");
                    s.last
                        .get(&c.id)
                        .copied()
                        .unwrap_or(if first { started } else { 0 })
                };
                followers.insert(
                    c.id.clone(),
                    tokio::spawn(follow(
                        runtime.clone(),
                        c.id,
                        c.deployment_id,
                        from,
                        shared.clone(),
                    )),
                );
            }
            first = false;
        }
        flush(&shared, &outbox);
    }
    for h in followers.values() {
        h.abort();
    }
}

async fn follow(
    runtime: Arc<dyn Runtime>,
    container: String,
    deployment: String,
    from_ms: i64,
    shared: Arc<Mutex<Shared>>,
) {
    let Ok(mut stream) = runtime.follow_since(&container, from_ms / 1000).await else {
        return;
    };
    while let Some(Ok(line)) = stream.next().await {
        // Docker resumes at a whole second: skip what was shipped already.
        if from_ms > 0 && line.ts_unix_ms <= from_ms {
            continue;
        }
        let mut s = shared.lock().expect("logship lock");
        s.last.insert(container.clone(), line.ts_unix_ms);
        if s.lines.len() >= BUFFER {
            s.dropped = s.dropped.saturating_add(1);
            continue;
        }
        s.lines.push(pb::ShippedLine {
            deployment_id: deployment.clone(),
            at_unix_ms: line.ts_unix_ms,
            stderr: line.stderr,
            text: truncate(line.text),
        });
    }
}

fn flush(shared: &Mutex<Shared>, outbox: &mpsc::Sender<pb::NodeMessage>) {
    loop {
        // Leave half the outbox to everything else (backup results must
        // never wait behind logs while the server is away).
        if outbox.capacity() * 2 < outbox.max_capacity() {
            return;
        }
        let batch = {
            let mut s = shared.lock().expect("logship lock");
            if s.lines.is_empty() && s.dropped == 0 {
                return;
            }
            let n = s.lines.len().min(PER_BATCH);
            pb::LogBatch {
                lines: s.lines.drain(..n).collect(),
                dropped: std::mem::take(&mut s.dropped),
            }
        };
        let more = batch.lines.len() == PER_BATCH;
        let m = pb::NodeMessage {
            msg: Some(pb::node_message::Msg::LogBatch(batch)),
        };
        if let Err(mpsc::error::TrySendError::Full(m) | mpsc::error::TrySendError::Closed(m)) =
            outbox.try_send(m)
        {
            // Put the lines back in front for the next round.
            if let Some(pb::node_message::Msg::LogBatch(b)) = m.msg {
                let mut s = shared.lock().expect("logship lock");
                let rest = std::mem::take(&mut s.lines);
                s.lines = b.lines;
                s.lines.extend(rest);
                s.dropped = s.dropped.saturating_add(b.dropped);
            }
            return;
        }
        if !more {
            return;
        }
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_LINE {
        let mut end = MAX_LINE;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_are_cut_on_a_char_boundary() {
        let s = "é".repeat(MAX_LINE);
        let t = truncate(s);
        assert!(t.len() <= MAX_LINE + '…'.len_utf8());
        assert!(t.ends_with('…'));
        assert_eq!(truncate("short".into()), "short");
    }

    #[test]
    fn a_full_outbox_keeps_the_lines() {
        let shared = Mutex::new(Shared::default());
        shared.lock().unwrap().lines.push(pb::ShippedLine {
            deployment_id: "d".into(),
            at_unix_ms: 1,
            stderr: false,
            text: "a".into(),
        });
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(pb::NodeMessage::default()).unwrap();
        flush(&shared, &tx);
        assert_eq!(shared.lock().unwrap().lines.len(), 1);
        rx.try_recv().unwrap();
        flush(&shared, &tx);
        assert!(shared.lock().unwrap().lines.is_empty());
        match rx.try_recv().unwrap().msg {
            Some(pb::node_message::Msg::LogBatch(b)) => assert_eq!(b.lines[0].text, "a"),
            other => panic!("unexpected {other:?}"),
        }
    }
}
