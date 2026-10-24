//! Starting workerd: the real binary, sandboxed, or a fake for tests.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

/// One line of workerd's output: (stderr, text).
pub type Line = (bool, String);

/// Starts workerd serving the `config` constant of a config file.
#[async_trait]
pub trait Launcher: Send + Sync + 'static {
    /// Every line it prints goes to `output`.
    async fn launch(
        &self,
        config: &Path,
        output: mpsc::UnboundedSender<Line>,
    ) -> anyhow::Result<Box<dyn Process>>;

    /// Hands the files workerd reads (`path`, recursively) to the user it
    /// runs as. Nothing to do when it runs as this process's user.
    fn own(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }

    /// Whether workerd runs as a user of its own (which must then be able
    /// to reach the functions' directory).
    fn own_user(&self) -> bool {
        false
    }
}

/// A running workerd.
#[async_trait]
pub trait Process: Send {
    /// Its exit code once it exited (-1: killed by a signal).
    async fn wait(&mut self) -> i32;
    /// SIGTERM, then SIGKILL after a grace period; returns once it exited.
    async fn stop(&mut self);
}

/// The installer's unprivileged user for workerd.
pub const USER: &str = "modula-fn";
const STOP_GRACE: Duration = Duration::from_secs(5);

/// The pinned workerd binary, run as [`USER`] when this node runs as root
/// and that user exists, inside bubblewrap when it works on this host.
pub struct Workerd {
    bin: PathBuf,
    #[cfg(unix)]
    user: Option<crate::build::buildkit::User>,
    bwrap: Option<PathBuf>,
}

impl Workerd {
    pub async fn new(bin: PathBuf) -> Self {
        #[cfg(unix)]
        let user = {
            use std::os::unix::fs::MetadataExt as _;
            let root = std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0);
            let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
            root.then(|| crate::build::buildkit::lookup_user(&passwd, USER))
                .flatten()
        };
        #[cfg(unix)]
        match user {
            Some(u) => {
                tracing::info!(user = USER, uid = u.uid, "functions run as their own user");
                crate::runtime::firewall::isolate_uid(u.uid).await;
            }
            None => {
                tracing::warn!("functions run as this process's user (no {USER} user, or not root)")
            }
        }
        let mut w = Self {
            bin,
            #[cfg(unix)]
            user,
            bwrap: None,
        };
        w.bwrap = w.find_bwrap().await;
        w
    }

    /// bubblewrap, when it is installed and can make a sandbox here (it
    /// needs unprivileged user namespaces when not run as root).
    async fn find_bwrap(&self) -> Option<PathBuf> {
        let bin = ["/usr/bin/bwrap", "/bin/bwrap"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())?;
        let mut cmd = self.command(&bin, &["--ro-bind", "/", "/", "--", "/bin/true"]);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        match tokio::time::timeout(Duration::from_secs(5), cmd.status()).await {
            Ok(Ok(s)) if s.success() => {
                tracing::info!("functions run in a bubblewrap sandbox");
                Some(bin)
            }
            _ => {
                tracing::warn!(
                    "bubblewrap can't sandbox functions on this host; running without it"
                );
                None
            }
        }
    }

    /// `program args`, with a cleared environment, as the functions' user.
    fn command(&self, program: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        if let Some(u) = self.user {
            cmd.uid(u.uid).gid(u.gid);
        }
        cmd
    }
}

/// bubblewrap's arguments: a new PID, IPC and UTS namespace (the host's
/// network: workerd listens on its loopback), the system read-only, only
/// workerd and its config directory besides, nothing writable but a /tmp of
/// its own, gone with its parent.
pub fn bwrap_args(bin: &Path, dir: &Path, config: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "--die-with-parent",
        "--new-session",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--ro-bind",
        "/usr",
        "/usr",
        "--ro-bind-try",
        "/lib",
        "/lib",
        "--ro-bind-try",
        "/lib64",
        "/lib64",
        "--ro-bind-try",
        "/etc/resolv.conf",
        "/etc/resolv.conf",
        "--ro-bind-try",
        "/etc/hosts",
        "/etc/hosts",
        "--ro-bind-try",
        "/etc/nsswitch.conf",
        "/etc/nsswitch.conf",
        "--ro-bind-try",
        "/etc/ssl",
        "/etc/ssl",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let dir = dir.display().to_string();
    a.extend([
        "--ro-bind".into(),
        bin.display().to_string(),
        "/run/workerd".into(),
        "--ro-bind".into(),
        dir.clone(),
        dir,
        "--".into(),
        "/run/workerd".into(),
    ]);
    a.extend(serve_args(config));
    a
}

/// `serve <config> config`: the `config` constant of the file.
pub fn serve_args(config: &Path) -> Vec<String> {
    vec![
        "serve".into(),
        config.display().to_string(),
        "config".into(),
    ]
}

#[async_trait]
impl Launcher for Workerd {
    async fn launch(
        &self,
        config: &Path,
        output: mpsc::UnboundedSender<Line>,
    ) -> anyhow::Result<Box<dyn Process>> {
        let dir = config
            .parent()
            .ok_or_else(|| anyhow::anyhow!("the config has no directory"))?;
        let mut cmd = match &self.bwrap {
            Some(bwrap) => {
                let args = bwrap_args(&self.bin, dir, config);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                self.command(bwrap, &args)
            }
            None => {
                let args = serve_args(config);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                self.command(&self.bin, &args)
            }
        };
        cmd.current_dir(dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot start workerd ({}): {e}", self.bin.display()))?;
        if let Some(out) = child.stdout.take() {
            tokio::spawn(lines(out, false, output.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(lines(err, true, output));
        }
        Ok(Box::new(Running(child)))
    }

    #[cfg(unix)]
    fn own(&self, path: &Path) -> std::io::Result<()> {
        fn walk(p: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
            std::os::unix::fs::lchown(p, Some(uid), Some(gid))?;
            if std::fs::symlink_metadata(p)?.is_dir() {
                for e in std::fs::read_dir(p)? {
                    walk(&e?.path(), uid, gid)?;
                }
            }
            Ok(())
        }
        match self.user {
            Some(u) => walk(path, u.uid, u.gid),
            None => Ok(()),
        }
    }

    #[cfg(unix)]
    fn own_user(&self) -> bool {
        self.user.is_some()
    }
}

async fn lines(r: impl AsyncRead + Unpin, stderr: bool, out: mpsc::UnboundedSender<Line>) {
    let mut r = BufReader::new(r).lines();
    while let Ok(Some(l)) = r.next_line().await {
        if out.send((stderr, l)).is_err() {
            return;
        }
    }
}

struct Running(Child);

#[async_trait]
impl Process for Running {
    async fn wait(&mut self) -> i32 {
        match self.0.wait().await {
            Ok(s) => s.code().unwrap_or(-1),
            Err(_) => -1,
        }
    }

    async fn stop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.id() {
            // Lets workerd finish in-flight requests.
            let _ = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .env_clear()
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            if tokio::time::timeout(STOP_GRACE, self.0.wait())
                .await
                .is_ok()
            {
                return;
            }
        }
        let _ = self.0.kill().await;
    }
}

/// Stands in for workerd (tests): records every config it was started
/// with and "listens" on the config's sockets until stopped. A function
/// whose code contains `FAIL_MARK` fails to load, like a syntax error:
/// the process prints an error and exits 1.
#[derive(Default)]
pub struct FakeWorkerd {
    pub launches: Mutex<Vec<String>>,
    pub running: Arc<Mutex<usize>>,
}

pub const FAIL_MARK: &str = "fake-workerd-fails-to-load";

impl FakeWorkerd {
    pub fn launches(&self) -> Vec<String> {
        self.launches.lock().expect("fake lock").clone()
    }
}

#[async_trait]
impl Launcher for FakeWorkerd {
    async fn launch(
        &self,
        config: &Path,
        output: mpsc::UnboundedSender<Line>,
    ) -> anyhow::Result<Box<dyn Process>> {
        let text = std::fs::read_to_string(config)?;
        self.launches.lock().expect("fake lock").push(text.clone());
        let dir = config.parent().unwrap_or(Path::new("/")).to_owned();
        let broken = text
            .lines()
            .filter_map(|l| l.split("embed \"").nth(1)?.split('"').next())
            .any(|m| std::fs::read_to_string(dir.join(m)).is_ok_and(|c| c.contains(FAIL_MARK)));
        let (stop_tx, stop_rx) = oneshot::channel();
        if broken {
            let _ = output.send((true, "Uncaught SyntaxError: Unexpected token".into()));
            let _ = stop_tx.send(());
            return Ok(Box::new(Fake {
                done: Some(stop_rx),
                stop: None,
                code: 1,
            }));
        }
        let mut listeners = Vec::new();
        for addr in text
            .lines()
            .filter_map(|l| l.split("address = \"").nth(1)?.split('"').next())
        {
            listeners.push(tokio::net::TcpListener::bind(addr).await?);
        }
        let running = self.running.clone();
        *running.lock().expect("fake lock") += 1;
        let (done_tx, done_rx) = oneshot::channel();
        tokio::spawn(async move {
            let serve = async {
                loop {
                    for l in &listeners {
                        if let Ok(Ok((mut s, _))) =
                            tokio::time::timeout(Duration::from_millis(5), l.accept()).await
                        {
                            use tokio::io::AsyncWriteExt as _;
                            let _ = s
                                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                                .await;
                        }
                    }
                }
            };
            tokio::select! {
                _ = stop_rx => {}
                _ = serve => {}
            }
            *running.lock().expect("fake lock") -= 1;
            let _ = done_tx.send(());
        });
        Ok(Box::new(Fake {
            done: Some(done_rx),
            stop: Some(stop_tx),
            code: 0,
        }))
    }
}

struct Fake {
    done: Option<oneshot::Receiver<()>>,
    stop: Option<oneshot::Sender<()>>,
    code: i32,
}

#[async_trait]
impl Process for Fake {
    async fn wait(&mut self) -> i32 {
        // Cancel-safe: the receiver stays until it fired.
        if let Some(d) = &mut self.done {
            let _ = d.await;
            self.done = None;
        }
        self.code
    }

    async fn stop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        self.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_sees_workerd_and_its_config_only() {
        let a = bwrap_args(
            Path::new("/opt/modula/current/workerd"),
            Path::new("/var/lib/modula/functions"),
            Path::new("/var/lib/modula/functions/workerd.capnp"),
        );
        let s = a.join(" ");
        assert!(s.starts_with("--die-with-parent --new-session --unshare-pid"));
        assert!(
            !s.contains("--unshare-net"),
            "it listens on the host's loopback"
        );
        assert!(s.contains("--ro-bind /opt/modula/current/workerd /run/workerd"));
        assert!(s.contains("--ro-bind /var/lib/modula/functions /var/lib/modula/functions"));
        assert!(!s.contains("--bind "), "nothing writable but its own /tmp");
        assert!(
            s.ends_with("-- /run/workerd serve /var/lib/modula/functions/workerd.capnp config")
        );
    }
}
