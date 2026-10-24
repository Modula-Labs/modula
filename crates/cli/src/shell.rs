//! `modula ssh`: a shell or command in a deployment's running container,
//! over the WebSocket `/api/deployments/{id}/exec` (see the server's `exec`
//! module for the frames).

use std::io::{IsTerminal, Read, Write};

use anyhow::Context;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use url::Url;

use crate::config::Login;
use crate::tunnel::{Target, Ws};

/// First byte of every output frame.
const STDOUT: u8 = 1;
const STDERR: u8 = 2;

/// What to run and how.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOptions {
    /// argv; empty = a login shell.
    pub cmd: Vec<String>,
    pub tty: bool,
    pub cols: u16,
    pub rows: u16,
    /// Which replica (default: the first active one).
    pub replica: Option<u16>,
}

/// `https://host/` → `wss://host/api/deployments/<id>/exec?…`.
pub fn exec_url(base: &Url, deployment: &str, opts: &ExecOptions) -> anyhow::Result<Url> {
    let mut url = crate::tunnel::ws_url(base, &format!("api/deployments/{deployment}/exec"))?;
    {
        let mut q = url.query_pairs_mut();
        if !opts.cmd.is_empty() {
            q.append_pair("cmd", &serde_json::to_string(&opts.cmd)?);
        }
        q.append_pair("tty", if opts.tty { "true" } else { "false" });
        if opts.tty && opts.cols > 0 && opts.rows > 0 {
            q.append_pair("cols", &opts.cols.to_string());
            q.append_pair("rows", &opts.rows.to_string());
        }
        if let Some(r) = opts.replica {
            q.append_pair("replica", &r.to_string());
        }
    }
    Ok(url)
}

/// What arrives from the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The end: the exit code (`None`: it did not exit by itself) and why.
    Exit {
        code: Option<i32>,
        reason: String,
    },
}

#[derive(Deserialize)]
struct ExitFrame {
    code: Option<i32>,
    #[serde(default)]
    reason: String,
}

/// One running command.
pub struct Exec {
    sink: SplitSink<Ws, Message>,
    stream: SplitStream<Ws>,
}

impl Exec {
    /// Start `opts` in `deployment`'s container. Errors (not running, not
    /// allowed, …) come back as the API's own.
    pub async fn open(login: &Login, deployment: &str, opts: &ExecOptions) -> anyhow::Result<Self> {
        let target = Target::at(login, exec_url(&login.url, deployment, opts)?)?;
        let (sink, stream) = target.open().await?.split();
        Ok(Self { sink, stream })
    }

    pub async fn stdin(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.sink
            .send(Message::Binary(data.to_vec().into()))
            .await
            .context("the connection closed")
    }

    /// Stdin ends (the command sees EOF).
    pub async fn eof(&mut self) -> anyhow::Result<()> {
        self.control(serde_json::json!({ "type": "eof" })).await
    }

    pub async fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.control(serde_json::json!({ "type": "resize", "cols": cols, "rows": rows }))
            .await
    }

    async fn control(&mut self, v: serde_json::Value) -> anyhow::Result<()> {
        self.sink
            .send(Message::Text(v.to_string().into()))
            .await
            .context("the connection closed")
    }

    /// The next output, ending with [`Event::Exit`].
    pub async fn next(&mut self) -> Event {
        while let Some(msg) = self.stream.next().await {
            match msg {
                Ok(Message::Binary(b)) => match b.split_first() {
                    Some((&STDERR, rest)) => return Event::Stderr(rest.to_vec()),
                    Some((&STDOUT, rest)) => return Event::Stdout(rest.to_vec()),
                    _ => {}
                },
                Ok(Message::Text(t)) => {
                    if let Ok(f) = serde_json::from_str::<ExitFrame>(&t) {
                        return Event::Exit {
                            code: f.code,
                            reason: f.reason,
                        };
                    }
                }
                Ok(Message::Close(frame)) => {
                    let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                    return Event::Exit { code: None, reason };
                }
                Ok(_) => {}
                Err(e) => {
                    return Event::Exit {
                        code: None,
                        reason: format!("the connection failed: {e}"),
                    };
                }
            }
        }
        Event::Exit {
            code: None,
            reason: "the connection closed".into(),
        }
    }
}

/// Puts the terminal in raw mode until dropped (also on errors and panics
/// that unwind).
struct RawMode;

impl RawMode {
    fn enable() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode().context("cannot switch the terminal to raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Whether to give the command a terminal: only when we have one.
pub fn wants_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Run `opts` attached to this process's stdin, stdout and stderr; returns
/// the command's exit code.
pub async fn attach(login: &Login, deployment: &str, mut opts: ExecOptions) -> anyhow::Result<i32> {
    if opts.tty {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        (opts.cols, opts.rows) = (cols, rows);
    }
    let mut exec = Exec::open(login, deployment, &opts).await?;
    let raw = if opts.tty {
        Some(RawMode::enable()?)
    } else {
        None
    };

    // Stdin on its own thread: a blocked read must not keep the process
    // alive once the command exited.
    let (input_tx, mut input) = mpsc::channel::<Option<Vec<u8>>>(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = input_tx.blocking_send(None);
                    return;
                }
                Ok(n) => {
                    if input_tx.blocking_send(Some(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    #[cfg(unix)]
    let mut winch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok();

    let mut stdin_open = true;
    let (code, reason) = loop {
        #[cfg(unix)]
        let resized = async {
            match winch.as_mut() {
                Some(w) if opts.tty => w.recv().await,
                _ => std::future::pending().await,
            }
        };
        #[cfg(not(unix))]
        let resized = std::future::pending::<Option<()>>();
        tokio::select! {
            event = exec.next() => match event {
                Event::Stdout(b) => {
                    let mut out = std::io::stdout().lock();
                    out.write_all(&b)?;
                    out.flush()?;
                }
                Event::Stderr(b) => {
                    let mut err = std::io::stderr().lock();
                    err.write_all(&b)?;
                    err.flush()?;
                }
                Event::Exit { code, reason } => break (code, reason),
            },
            data = input.recv(), if stdin_open => match data.flatten() {
                Some(b) => exec.stdin(&b).await?,
                None => {
                    stdin_open = false;
                    exec.eof().await?;
                }
            },
            Some(()) = resized => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    exec.resize(cols, rows).await?;
                }
            }
        }
    };
    drop(raw);
    match code {
        Some(c) => Ok(c),
        None => {
            if !reason.is_empty() {
                eprintln!("\r\nconnection closed: {reason}");
            }
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_urls_carry_the_command_as_json() {
        let base = Url::parse("https://cloud.example.com/").unwrap();
        let shell = ExecOptions {
            tty: true,
            cols: 120,
            rows: 40,
            ..Default::default()
        };
        assert_eq!(
            exec_url(&base, "d1", &shell).unwrap().as_str(),
            "wss://cloud.example.com/api/deployments/d1/exec?tty=true&cols=120&rows=40"
        );
        let cmd = ExecOptions {
            cmd: vec!["ls".into(), "-la".into()],
            replica: Some(1),
            ..Default::default()
        };
        let url = exec_url(&Url::parse("http://localhost:8080/").unwrap(), "d1", &cmd).unwrap();
        assert_eq!(url.scheme(), "ws");
        assert_eq!(url.path(), "/api/deployments/d1/exec");
        let q: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(
            q,
            [
                ("cmd".into(), r#"["ls","-la"]"#.into()),
                ("tty".into(), "false".into()),
                ("replica".into(), "1".into()),
            ]
        );
    }
}
