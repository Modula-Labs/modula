//! `docker exec`: streamed stdout or stdin for database dumps and restores
//! (argv lists chosen by the node, never shell strings), and interactive
//! commands a user runs (`exec_interactive`, see [`crate::exec`]).

use std::time::Duration;

use anyhow::{Context, bail};
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::{ResizeExecOptions, StartExecOptions, StartExecResults};
use bollard::models::ExecConfig;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::{Exec, ExecControl, ExecOutput, ExecSession, ExecSpec};

/// How much of a failing command's output ends up in its error.
const OUTPUT_TAIL: usize = 1500;

pub(super) async fn container_env(
    docker: &Docker,
    container_id: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let info = docker.inspect_container(container_id, None).await?;
    Ok(info
        .config
        .and_then(|c| c.env)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
        })
        .collect())
}

type Output = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<LogOutput, bollard::errors::Error>> + Send>,
>;
type Input = std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>;

async fn start(
    docker: &Docker,
    container_id: &str,
    exec: Exec,
    stdin: bool,
) -> anyhow::Result<(String, Output, Input)> {
    let created = docker
        .create_exec(
            container_id,
            ExecConfig {
                attach_stdin: Some(stdin),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(exec.cmd),
                env: Some(exec.env),
                ..Default::default()
            },
        )
        .await
        .context("cannot start the command in the container")?;
    let opts = StartExecOptions {
        detach: false,
        tty: false,
        output_capacity: None,
    };
    match docker.start_exec(&created.id, Some(opts)).await? {
        StartExecResults::Attached { output, input } => Ok((created.id, output, input)),
        StartExecResults::Detached => bail!("docker did not attach to the command"),
    }
}

pub(super) async fn exec_output(
    docker: &Docker,
    container_id: &str,
    exec: Exec,
) -> anyhow::Result<BoxStream<'static, anyhow::Result<Bytes>>> {
    let program = exec.cmd.first().cloned().unwrap_or_default();
    let (exec_id, mut output, _input) = start(docker, container_id, exec, false).await?;
    let (tx, rx) = mpsc::channel(8);
    let docker = docker.clone();
    tokio::spawn(async move {
        let mut stderr = Tail::default();
        while let Some(item) = output.next().await {
            match item {
                Ok(LogOutput::StdOut { message }) => {
                    // The reader is gone (the upload failed): stop reading,
                    // which closes the exec's output and ends the command.
                    if tx.send(Ok(message)).await.is_err() {
                        return;
                    }
                }
                Ok(LogOutput::StdErr { message }) => stderr.push(&message),
                Ok(_) => {}
                Err(e) => {
                    let _ = tx.send(Err(e.into())).await;
                    return;
                }
            }
        }
        if let Err(e) = exit_status(&docker, &exec_id, &program, &stderr).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    Ok(ReceiverStream::new(rx).boxed())
}

pub(super) async fn exec_input(
    docker: &Docker,
    container_id: &str,
    exec: Exec,
    mut stdin: BoxStream<'static, anyhow::Result<Bytes>>,
) -> anyhow::Result<()> {
    let program = exec.cmd.first().cloned().unwrap_or_default();
    let (exec_id, mut output, mut input) = start(docker, container_id, exec, true).await?;
    // Drain the output concurrently so a chatty command never blocks on a
    // full pipe while we are still feeding it.
    let drain = tokio::spawn(async move {
        let mut tail = Tail::default();
        while let Some(item) = output.next().await {
            match item? {
                LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                    tail.push(&message)
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(tail)
    });
    // Where feeding stopped: the input failing (download or decryption) is
    // the error to report; the command no longer reading means it exited,
    // and its own exit status explains why.
    let mut write_error = None;
    while let Some(chunk) = stdin.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                // Dropping both halves closes the connection and the
                // command's stdin.
                drain.abort();
                return Err(e);
            }
        };
        if let Err(e) = input.write_all(&chunk).await {
            write_error = Some(e);
            break;
        }
    }
    if write_error.is_none() {
        // End of input: the command sees EOF.
        write_error = input.shutdown().await.err();
    }
    drop(input);
    let tail = drain.await.context("output reader panicked")??;
    exit_status(docker, &exec_id, &program, &tail).await?;
    match write_error {
        Some(e) => Err(anyhow::Error::from(e).context("the command stopped reading its input")),
        None => Ok(()),
    }
}

async fn exit_status(
    docker: &Docker,
    exec_id: &str,
    program: &str,
    output: &Tail,
) -> anyhow::Result<()> {
    // The output can end a moment before Docker records the exit code.
    for _ in 0..50 {
        let info = docker.inspect_exec(exec_id).await?;
        if info.running != Some(true) {
            return match info.exit_code {
                Some(0) => Ok(()),
                // Docker's own codes: the image lacks the tool.
                Some(126 | 127) => bail!("{program} is not available in the container"),
                Some(code) => bail!("{program} failed (exit code {code}): {}", output.text()),
                None => bail!("{program} failed (unknown exit status): {}", output.text()),
            };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("the command did not exit")
}

/// The last few hundred bytes a command printed, for its error message.
#[derive(Default)]
struct Tail(Vec<u8>);

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
        if self.0.len() > OUTPUT_TAIL {
            self.0.drain(..self.0.len() - OUTPUT_TAIL);
        }
    }

    fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.0);
        let text = text.trim();
        if text.is_empty() {
            "no output".into()
        } else {
            text.to_owned()
        }
    }
}

pub(super) async fn exec_interactive(
    docker: &Docker,
    container_id: &str,
    spec: ExecSpec,
) -> anyhow::Result<ExecSession> {
    let console_size = (spec.tty && spec.cols > 0 && spec.rows > 0)
        .then(|| vec![usize::from(spec.rows), usize::from(spec.cols)]);
    let created = docker
        .create_exec(
            container_id,
            ExecConfig {
                attach_stdin: Some(true),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                tty: Some(spec.tty),
                console_size,
                cmd: Some(spec.cmd),
                ..Default::default()
            },
        )
        .await
        .context("cannot start the command in the container")?;
    let opts = StartExecOptions {
        detach: false,
        tty: spec.tty,
        output_capacity: None,
    };
    let (output, input) = match docker.start_exec(&created.id, Some(opts)).await? {
        StartExecResults::Attached { output, input } => (output, input),
        StartExecResults::Detached => bail!("docker did not attach to the command"),
    };
    let output = output
        .filter_map(|item| async move {
            match item {
                Ok(LogOutput::StdErr { message }) => Some(Ok(ExecOutput {
                    stderr: true,
                    data: message,
                })),
                Ok(LogOutput::StdOut { message } | LogOutput::Console { message }) => {
                    Some(Ok(ExecOutput {
                        stderr: false,
                        data: message,
                    }))
                }
                Ok(LogOutput::StdIn { .. }) => None,
                Err(e) => Some(Err(e.into())),
            }
        })
        .boxed();
    Ok(ExecSession {
        output,
        input,
        control: Box::new(Control {
            docker: docker.clone(),
            exec_id: created.id,
        }),
    })
}

struct Control {
    docker: Docker,
    exec_id: String,
}

#[async_trait::async_trait]
impl ExecControl for Control {
    async fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.docker
            .resize_exec(
                &self.exec_id,
                ResizeExecOptions {
                    width: cols,
                    height: rows,
                },
            )
            .await?;
        Ok(())
    }

    async fn exit_code(&self) -> anyhow::Result<Option<i64>> {
        // The output can end a moment before Docker records the exit code.
        for _ in 0..50 {
            let info = self.docker.inspect_exec(&self.exec_id).await?;
            if info.running != Some(true) {
                return Ok(info.exit_code);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(None)
    }
}
