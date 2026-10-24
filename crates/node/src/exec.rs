//! The node's end of interactive commands in containers (`modula ssh`, the
//! dashboard's Shell tab).
//!
//! The control plane asks to run a command in one deployment's running
//! container; the node starts it with the runtime's interactive exec and
//! pipes stdin and output over the session stream. Flow control is the
//! tunnels' (see [`crate::tunnel`]): at most [`WINDOW`] bytes in flight per
//! direction, credit returned as bytes are written to the command. Execs
//! belong to one session and end with it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use modula_proto::node::v1 as pb;
use pb::node_message::Msg as Out;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::runtime::{ExecSession, ExecSpec, Runtime};
use crate::tunnel::{CHUNK, Credit, WINDOW};

const CREDIT_EVERY: usize = WINDOW / 4;
const MAX_EXECS: usize = 32;

/// What runs when the user names no command: a login shell, bash when the
/// image has it.
pub const DEFAULT_SHELL: [&str; 3] = [
    "sh",
    "-lc",
    "command -v bash >/dev/null && exec bash || exec sh",
];

enum Input {
    /// Stdin bytes; `true` = stdin ends after them.
    Data(Vec<u8>, bool),
    Resize(u16, u16),
}

struct Running {
    input: mpsc::UnboundedSender<Input>,
    /// Credit for our sends to the server.
    credit: Arc<Credit>,
    /// Received but not yet written (≤ WINDOW).
    buffered: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

type Map = Arc<Mutex<HashMap<String, Running>>>;

/// This session's running execs. Dropping it ends them all.
pub struct Execs {
    runtime: Arc<dyn Runtime>,
    tx: mpsc::Sender<pb::NodeMessage>,
    open: Map,
}

fn msg(m: Out) -> pb::NodeMessage {
    pb::NodeMessage { msg: Some(m) }
}

fn close_msg(exec_id: String, reason: impl Into<String>) -> pb::NodeMessage {
    msg(Out::ExecClose(pb::ExecClose {
        exec_id,
        exited: false,
        exit_code: 0,
        reason: reason.into(),
    }))
}

fn opened(exec_id: String, error: Option<String>) -> pb::NodeMessage {
    msg(Out::ExecOpened(pb::ExecOpened {
        exec_id,
        ok: error.is_none(),
        error: error.unwrap_or_default(),
    }))
}

/// The command to run for `req`.
pub fn spec(req: &pb::ExecOpen) -> ExecSpec {
    let cmd = if req.cmd.is_empty() {
        DEFAULT_SHELL.iter().map(|s| (*s).to_owned()).collect()
    } else {
        req.cmd.clone()
    };
    let size = |n: u32| u16::try_from(n.min(1000)).unwrap_or(0);
    ExecSpec {
        cmd,
        tty: req.tty,
        cols: size(req.cols),
        rows: size(req.rows),
    }
}

impl Execs {
    pub fn new(runtime: Arc<dyn Runtime>, tx: mpsc::Sender<pb::NodeMessage>) -> Self {
        Self {
            runtime,
            tx,
            open: Arc::default(),
        }
    }

    /// Start the command; the answer (`ExecOpened`) is sent from a task so
    /// the session loop never waits on a container.
    pub fn open(&self, req: pb::ExecOpen) {
        let mut open = self.open.lock().expect("execs lock");
        if open.len() >= MAX_EXECS || open.contains_key(&req.exec_id) {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let error = "too many shells are open on this server".to_owned();
                let _ = tx.send(opened(req.exec_id, Some(error))).await;
            });
            return;
        }
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let credit = Arc::new(Credit::new());
        let buffered = Arc::new(AtomicUsize::new(0));
        let id = req.exec_id.clone();
        let task = tokio::spawn(run(
            req,
            self.runtime.clone(),
            self.tx.clone(),
            input_rx,
            credit.clone(),
            buffered.clone(),
            self.open.clone(),
        ));
        open.insert(
            id,
            Running {
                input: input_tx,
                credit,
                buffered,
                task,
            },
        );
    }

    pub fn data(&self, m: pb::ExecData) {
        let mut open = self.open.lock().expect("execs lock");
        let Some(e) = open.get(&m.exec_id) else {
            return;
        };
        if m.credit > 0 {
            e.credit.give(m.credit as usize);
        }
        if m.data.is_empty() && !m.eof {
            return;
        }
        let len = m.data.len();
        let over = len > CHUNK || e.buffered.fetch_add(len, Ordering::AcqRel) + len > WINDOW;
        if over || e.input.send(Input::Data(m.data, m.eof)).is_err() {
            if let Some(e) = open.remove(&m.exec_id) {
                e.task.abort();
            }
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(close_msg(m.exec_id, "flow control violated")).await;
            });
        }
    }

    pub fn resize(&self, m: pb::ExecResize) {
        let open = self.open.lock().expect("execs lock");
        if let Some(e) = open.get(&m.exec_id) {
            let size = |n: u32| u16::try_from(n.clamp(1, 1000)).unwrap_or(1);
            let _ = e.input.send(Input::Resize(size(m.cols), size(m.rows)));
        }
    }

    /// The server closed the exec: drop the command's connection.
    pub fn close(&self, m: pb::ExecClose) {
        if let Some(e) = self.open.lock().expect("execs lock").remove(&m.exec_id) {
            e.task.abort();
        }
    }

    /// Number of running execs (tests).
    pub fn len(&self) -> usize {
        self.open.lock().expect("execs lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for Execs {
    fn drop(&mut self) {
        for (_, e) in self.open.lock().expect("execs lock").drain() {
            e.task.abort();
        }
    }
}

async fn run(
    req: pb::ExecOpen,
    runtime: Arc<dyn Runtime>,
    tx: mpsc::Sender<pb::NodeMessage>,
    input: mpsc::UnboundedReceiver<Input>,
    credit: Arc<Credit>,
    buffered: Arc<AtomicUsize>,
    open: Map,
) {
    let id = req.exec_id.clone();
    let session = match start(&runtime, &req).await {
        Ok(s) => s,
        Err(e) => {
            open.lock().expect("execs lock").remove(&id);
            let _ = tx.send(opened(id, Some(format!("{e:#}")))).await;
            return;
        }
    };
    if tx.send(opened(id.clone(), None)).await.is_err() {
        return;
    }
    tracing::info!(exec = %id, deployment = %req.deployment_id, tty = req.tty, "exec started");
    let close = pump(&id, session, &tx, input, &credit, &buffered).await;
    tracing::info!(exec = %id, exited = close.exited, code = close.exit_code, "exec ended");
    open.lock().expect("execs lock").remove(&id);
    let _ = tx.send(msg(Out::ExecClose(close))).await;
}

async fn start(runtime: &Arc<dyn Runtime>, req: &pb::ExecOpen) -> anyhow::Result<ExecSession> {
    let container = runtime
        .containers()
        .await?
        .into_iter()
        .find(|c| c.deployment_id == req.deployment_id && c.running)
        .ok_or_else(|| anyhow::anyhow!("the deployment is not running on this server"))?;
    runtime.exec_interactive(&container.id, spec(req)).await
}

/// Pipe stdin in and output out until the command's output ends, then
/// report how it exited.
async fn pump(
    id: &str,
    session: ExecSession,
    tx: &mpsc::Sender<pb::NodeMessage>,
    mut input: mpsc::UnboundedReceiver<Input>,
    credit: &Credit,
    buffered: &AtomicUsize,
) -> pb::ExecClose {
    let ExecSession {
        mut output,
        input: stdin,
        control,
    } = session;
    let control = &*control;
    // Server → command. Keeps returning credit after the command stopped
    // reading, so the server never stalls; ends only with the exec.
    let to_command = async {
        let mut stdin = Some(stdin);
        let mut owed = 0usize;
        while let Some(m) = input.recv().await {
            let (bytes, eof) = match m {
                Input::Resize(cols, rows) => {
                    if let Err(e) = control.resize(cols, rows).await {
                        tracing::debug!(exec = %id, error = %e, "resize failed");
                    }
                    continue;
                }
                Input::Data(bytes, eof) => (bytes, eof),
            };
            if let Some(w) = stdin.as_mut()
                && !bytes.is_empty()
                && w.write_all(&bytes).await.is_err()
            {
                stdin = None;
            }
            if eof && let Some(mut w) = stdin.take() {
                let _ = w.shutdown().await;
            }
            buffered.fetch_sub(bytes.len(), Ordering::AcqRel);
            owed += bytes.len();
            if owed >= CREDIT_EVERY {
                let m = msg(Out::ExecData(pb::ExecData {
                    exec_id: id.to_owned(),
                    credit: std::mem::take(&mut owed) as u32,
                    ..Default::default()
                }));
                if tx.send(m).await.is_err() {
                    break;
                }
            }
        }
        std::future::pending::<()>().await;
    };
    // Command → server, waiting for credit.
    let from_command = async {
        while let Some(item) = output.next().await {
            let out = match item {
                Ok(out) => out,
                Err(e) => return Some(format!("reading the command's output failed: {e:#}")),
            };
            for chunk in out.data.chunks(CHUNK) {
                credit.take(chunk.len()).await;
                let m = msg(Out::ExecData(pb::ExecData {
                    exec_id: id.to_owned(),
                    data: chunk.to_vec(),
                    stderr: out.stderr,
                    ..Default::default()
                }));
                if tx.send(m).await.is_err() {
                    return Some("the control plane went away".to_owned());
                }
            }
        }
        None
    };
    let failed = tokio::select! {
        () = to_command => None,
        r = from_command => r,
    };
    let code = match failed {
        Some(_) => None,
        None => control.exit_code().await.ok().flatten(),
    };
    pb::ExecClose {
        exec_id: id.to_owned(),
        exited: code.is_some(),
        exit_code: code.map_or(0, |c| i32::try_from(c).unwrap_or(-1)),
        reason: failed.unwrap_or_else(|| match code {
            Some(c) => format!("exited with code {c}"),
            None => "the command ended".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeRuntime;
    use modula_proto::node::v1::Workload;

    fn open_req(id: &str, deployment: &str, cmd: &[&str]) -> pb::ExecOpen {
        pb::ExecOpen {
            exec_id: id.into(),
            deployment_id: deployment.into(),
            cmd: cmd.iter().map(|s| (*s).to_owned()).collect(),
            tty: true,
            cols: 100,
            rows: 30,
        }
    }

    #[test]
    fn no_command_means_a_login_shell() {
        let s = spec(&open_req("e", "d", &[]));
        assert_eq!(s.cmd, DEFAULT_SHELL);
        assert_eq!((s.cols, s.rows), (100, 30));
        let s = spec(&open_req("e", "d", &["ls", "-la"]));
        assert_eq!(s.cmd, ["ls", "-la"]);
    }

    async fn next(rx: &mut mpsc::Receiver<pb::NodeMessage>) -> Out {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a message in time")
            .and_then(|m| m.msg)
            .expect("a message")
    }

    #[tokio::test]
    async fn runs_a_command_and_reports_its_exit_code() {
        let rt = Arc::new(FakeRuntime::new());
        let w = Workload {
            deployment_id: "dep".into(),
            image: "app:1".into(),
            environment_id: "env".into(),
            ..Default::default()
        };
        rt.ensure_network("env").await.unwrap();
        let c = rt.create(&w).await.unwrap();
        rt.start(&c).await.unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let execs = Execs::new(rt.clone(), tx);

        execs.open(open_req("x", "missing", &[]));
        match next(&mut rx).await {
            Out::ExecOpened(o) => assert!(!o.ok && o.error.contains("not running")),
            other => panic!("unexpected {other:?}"),
        }

        execs.open(open_req("e", "dep", &[]));
        assert!(matches!(next(&mut rx).await, Out::ExecOpened(o) if o.ok));
        execs.resize(pb::ExecResize {
            exec_id: "e".into(),
            cols: 120,
            rows: 40,
        });
        execs.data(pb::ExecData {
            exec_id: "e".into(),
            data: b"hi\n".to_vec(),
            ..Default::default()
        });
        assert!(matches!(next(&mut rx).await, Out::ExecData(d) if d.data == b"hi\n"));
        execs.data(pb::ExecData {
            exec_id: "e".into(),
            data: b"exit 3\n".to_vec(),
            ..Default::default()
        });
        assert!(matches!(next(&mut rx).await, Out::ExecData(_)));
        match next(&mut rx).await {
            Out::ExecClose(c) => assert!(c.exited && c.exit_code == 3, "{c:?}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(execs.is_empty());
        assert_eq!(*rt.resizes.lock().unwrap(), [(120, 40)]);
        let sessions = rt.sessions.lock().unwrap();
        assert_eq!(sessions[0].spec.cmd, DEFAULT_SHELL);
    }

    #[tokio::test]
    async fn overrunning_the_window_closes_the_exec() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(16);
        let execs = Execs::new(rt, tx);
        execs.open(open_req("e", "none", &[]));
        execs.data(pb::ExecData {
            exec_id: "e".into(),
            data: vec![0; CHUNK + 1],
            ..Default::default()
        });
        assert!(execs.is_empty());
        loop {
            if let Out::ExecClose(c) = next(&mut rx).await {
                assert!(c.reason.contains("flow control"), "{c:?}");
                break;
            }
        }
    }
}
