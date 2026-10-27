//! The node's end of volume copies (migrating a server to another one).
//!
//! The control plane relays one transfer between two nodes: the source packs
//! a volume's directory as a gzipped tar and streams it as `VolumeData`; the
//! destination unpacks it into a fresh volume of the same name. Ownership,
//! permissions, symlinks and special files are kept, so a database's files
//! stay usable by its container's user. Flow control mirrors tunnels': the
//! source has at most [`WINDOW`] bytes in flight, and the destination returns
//! credit as it hands bytes to the unpacker.
//!
//! Neither side touches a volume that a container still mounts: the server
//! stops the service first, and this is checked here again. A missing source
//! volume (the service never ran) is sent as an empty archive. Transfers
//! belong to one session and end with it; the server then starts over.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use modula_proto::node::v1 as pb;
use pb::node_message::Msg as Out;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::runtime::Runtime;
use crate::tunnel::{CHUNK, Credit, WINDOW};

const CREDIT_EVERY: usize = WINDOW / 4;
const MAX_TRANSFERS: usize = 8;

/// What the destination's unpacker reads.
enum Chunk {
    Data(Vec<u8>),
    End,
}

enum Side {
    /// Sending: credit for our data.
    Export { credit: Arc<Credit> },
    /// Receiving: data for the unpacker, and what it hasn't taken yet.
    Import {
        data: mpsc::UnboundedSender<Chunk>,
        buffered: Arc<AtomicUsize>,
    },
}

struct Transfer {
    side: Side,
    task: JoinHandle<()>,
}

type Map = Arc<Mutex<HashMap<String, Transfer>>>;

/// This session's volume transfers. Dropping it ends them all.
pub struct Volumes {
    runtime: Arc<dyn Runtime>,
    tx: mpsc::Sender<pb::NodeMessage>,
    open: Map,
}

fn msg(m: Out) -> pb::NodeMessage {
    pb::NodeMessage { msg: Some(m) }
}

fn close_msg(transfer_id: String, result: Result<u64, String>) -> pb::NodeMessage {
    let (ok, error, bytes) = match result {
        Ok(b) => (true, String::new(), b),
        Err(e) => (false, e, 0),
    };
    msg(Out::VolumeClose(pb::VolumeClose {
        transfer_id,
        ok,
        error,
        bytes,
    }))
}

/// Only Modula's own volumes can be read or written this way.
fn valid_volume(name: &str) -> bool {
    name.strip_prefix("modula-vol-").is_some_and(|id| {
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

impl Volumes {
    pub fn new(runtime: Arc<dyn Runtime>, tx: mpsc::Sender<pb::NodeMessage>) -> Self {
        Self {
            runtime,
            tx,
            open: Arc::default(),
        }
    }

    /// Refuse a transfer without registering it; the answer goes out from a
    /// task so the session loop never waits.
    fn refuse(&self, transfer_id: String, why: &str) {
        let tx = self.tx.clone();
        let m = close_msg(transfer_id, Err(why.to_owned()));
        tokio::spawn(async move {
            let _ = tx.send(m).await;
        });
    }

    fn admit(&self, transfer_id: &str, volume: &str) -> Result<(), &'static str> {
        let open = self.open.lock().expect("volumes lock");
        if !valid_volume(volume) {
            return Err("not a Modula volume");
        }
        if open.len() >= MAX_TRANSFERS || open.contains_key(transfer_id) {
            return Err("too many volume copies are running on this server");
        }
        Ok(())
    }

    pub fn export(&self, req: pb::VolumeExport) {
        if let Err(why) = self.admit(&req.transfer_id, &req.volume) {
            return self.refuse(req.transfer_id, why);
        }
        let credit = Arc::new(Credit::new());
        let id = req.transfer_id.clone();
        let task = tokio::spawn(run_export(
            req,
            self.runtime.clone(),
            self.tx.clone(),
            credit.clone(),
            self.open.clone(),
        ));
        self.open.lock().expect("volumes lock").insert(
            id,
            Transfer {
                side: Side::Export { credit },
                task,
            },
        );
    }

    pub fn import(&self, req: pb::VolumeImport) {
        if let Err(why) = self.admit(&req.transfer_id, &req.volume) {
            return self.refuse(req.transfer_id, why);
        }
        let (data_tx, data_rx) = mpsc::unbounded_channel();
        let buffered = Arc::new(AtomicUsize::new(0));
        let id = req.transfer_id.clone();
        let task = tokio::spawn(run_import(
            req,
            self.runtime.clone(),
            self.tx.clone(),
            data_rx,
            buffered.clone(),
            self.open.clone(),
        ));
        self.open.lock().expect("volumes lock").insert(
            id,
            Transfer {
                side: Side::Import {
                    data: data_tx,
                    buffered,
                },
                task,
            },
        );
    }

    pub fn data(&self, m: pb::VolumeData) {
        let mut open = self.open.lock().expect("volumes lock");
        let Some(t) = open.get(&m.transfer_id) else {
            return;
        };
        let ok = match &t.side {
            Side::Export { credit } => {
                if m.credit > 0 {
                    credit.give(m.credit as usize);
                }
                true
            }
            Side::Import { data, buffered } => {
                let len = m.data.len();
                let over = len > CHUNK || buffered.fetch_add(len, Ordering::AcqRel) + len > WINDOW;
                !over
                    && (m.data.is_empty() || data.send(Chunk::Data(m.data)).is_ok())
                    && (!m.eof || data.send(Chunk::End).is_ok())
            }
        };
        if !ok {
            if let Some(t) = open.remove(&m.transfer_id) {
                t.task.abort();
            }
            self.refuse(m.transfer_id, "flow control violated");
        }
    }

    /// The server gave the transfer up.
    pub fn close(&self, m: pb::VolumeClose) {
        if let Some(t) = self
            .open
            .lock()
            .expect("volumes lock")
            .remove(&m.transfer_id)
        {
            t.task.abort();
        }
    }
}

impl Drop for Volumes {
    fn drop(&mut self) {
        for (_, t) in self.open.lock().expect("volumes lock").drain() {
            t.task.abort();
        }
    }
}

async fn in_use(runtime: &Arc<dyn Runtime>, volume: &str) -> Result<(), String> {
    let users = runtime
        .volume_users(volume)
        .await
        .map_err(|e| format!("{e:#}"))?;
    if users.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "volume {volume} is still mounted by a container; stop the service first"
        ))
    }
}

async fn run_export(
    req: pb::VolumeExport,
    runtime: Arc<dyn Runtime>,
    tx: mpsc::Sender<pb::NodeMessage>,
    credit: Arc<Credit>,
    open: Map,
) {
    let id = req.transfer_id.clone();
    let result = export(&req, &runtime, &tx, &credit).await;
    if let Err(e) = &result {
        tracing::warn!(volume = %req.volume, error = %e, "volume export failed");
        let _ = tx.send(close_msg(id.clone(), Err(e.clone()))).await;
    }
    open.lock().expect("volumes lock").remove(&id);
}

async fn export(
    req: &pb::VolumeExport,
    runtime: &Arc<dyn Runtime>,
    tx: &mpsc::Sender<pb::NodeMessage>,
    credit: &Credit,
) -> Result<u64, String> {
    in_use(runtime, &req.volume).await?;
    let dir = runtime
        .volume_path(&req.volume)
        .await
        .map_err(|e| format!("{e:#}"))?;
    if dir.is_none() {
        tracing::info!(volume = %req.volume, "volume never created here; sending it empty");
    }
    let (chunks_tx, mut chunks) = mpsc::channel::<Vec<u8>>(4);
    let packer = tokio::task::spawn_blocking(move || {
        let w = ChunkWriter::new(chunks_tx);
        match dir {
            Some(d) => pack(&d, w),
            None => pack_empty(w),
        }
    });
    let mut sent = 0u64;
    while let Some(data) = chunks.recv().await {
        credit.take(data.len()).await;
        sent += data.len() as u64;
        let m = msg(Out::VolumeData(pb::VolumeData {
            transfer_id: req.transfer_id.clone(),
            data,
            credit: 0,
            eof: false,
        }));
        if tx.send(m).await.is_err() {
            return Err("the session ended".into());
        }
    }
    packer
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("packing {}: {e}", req.volume))?;
    let end = msg(Out::VolumeData(pb::VolumeData {
        transfer_id: req.transfer_id.clone(),
        eof: true,
        ..Default::default()
    }));
    tx.send(end).await.map_err(|_| "the session ended")?;
    Ok(sent)
}

async fn run_import(
    req: pb::VolumeImport,
    runtime: Arc<dyn Runtime>,
    tx: mpsc::Sender<pb::NodeMessage>,
    data: mpsc::UnboundedReceiver<Chunk>,
    buffered: Arc<AtomicUsize>,
    open: Map,
) {
    let id = req.transfer_id.clone();
    let result = import(&req, &runtime, &tx, data, buffered).await;
    if let Err(e) = &result {
        tracing::warn!(volume = %req.volume, error = %e, "volume import failed");
    }
    open.lock().expect("volumes lock").remove(&id);
    let _ = tx.send(close_msg(id, result)).await;
}

async fn import(
    req: &pb::VolumeImport,
    runtime: &Arc<dyn Runtime>,
    tx: &mpsc::Sender<pb::NodeMessage>,
    data: mpsc::UnboundedReceiver<Chunk>,
    buffered: Arc<AtomicUsize>,
) -> Result<u64, String> {
    in_use(runtime, &req.volume).await?;
    let dir = runtime
        .create_volume(&req.volume)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let reader = ChunkReader {
        rx: data,
        cur: Vec::new(),
        pos: 0,
        ended: false,
        buffered,
        pending_credit: 0,
        total: 0,
        transfer_id: req.transfer_id.clone(),
        tx: tx.clone(),
    };
    let volume = req.volume.clone();
    tokio::task::spawn_blocking(move || -> io::Result<u64> {
        empty_dir(&dir)?;
        let mut reader = reader;
        unpack(&mut reader, &dir)?;
        // The gzip trailer and tar padding: read to the end.
        io::copy(&mut reader, &mut io::sink())?;
        Ok(reader.total)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("unpacking {volume}: {e}"))
}

/// Buffers writes into chunks of at most [`CHUNK`] for the sending task.
struct ChunkWriter {
    tx: mpsc::Sender<Vec<u8>>,
    buf: Vec<u8>,
}

impl ChunkWriter {
    fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            tx,
            buf: Vec::with_capacity(CHUNK),
        }
    }

    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        self.tx
            .blocking_send(chunk)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the transfer was cancelled"))
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = data.len().min(CHUNK - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        if self.buf.len() == CHUNK {
            self.send()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

/// Reads what the session hands over, returning credit as it goes.
struct ChunkReader {
    rx: mpsc::UnboundedReceiver<Chunk>,
    cur: Vec<u8>,
    pos: usize,
    ended: bool,
    buffered: Arc<AtomicUsize>,
    pending_credit: usize,
    total: u64,
    transfer_id: String,
    tx: mpsc::Sender<pb::NodeMessage>,
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.cur.len() {
            if self.ended {
                return Ok(0);
            }
            match self.rx.blocking_recv() {
                Some(Chunk::Data(d)) => {
                    let len = d.len();
                    self.buffered.fetch_sub(len, Ordering::AcqRel);
                    self.total += len as u64;
                    self.cur = d;
                    self.pos = 0;
                    self.pending_credit += len;
                    if self.pending_credit >= CREDIT_EVERY {
                        let credit = std::mem::take(&mut self.pending_credit) as u32;
                        let m = msg(Out::VolumeData(pb::VolumeData {
                            transfer_id: self.transfer_id.clone(),
                            credit,
                            ..Default::default()
                        }));
                        self.tx.blocking_send(m).map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "the session ended")
                        })?;
                    }
                }
                Some(Chunk::End) => self.ended = true,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the transfer was cancelled",
                    ));
                }
            }
        }
        let n = out.len().min(self.cur.len() - self.pos);
        out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Write `dir` as a gzipped tar: the directory itself first (as `./`, for
/// its owner and mode), then everything in it, symlinks not followed.
pub fn pack(dir: &Path, w: impl Write) -> io::Result<()> {
    let gz = flate2::write::GzEncoder::new(w, flate2::Compression::fast());
    let mut b = tar::Builder::new(gz);
    b.follow_symlinks(false);
    b.mode(tar::HeaderMode::Complete);
    b.append_dir_all(".", dir)?;
    b.into_inner()?.finish()?.flush()
}

fn pack_empty(w: impl Write) -> io::Result<()> {
    let gz = flate2::write::GzEncoder::new(w, flate2::Compression::fast());
    tar::Builder::new(gz).into_inner()?.finish()?.flush()
}

/// Unpack a [`pack`]ed archive into `dst`, keeping owners and modes.
/// Entries that would land outside `dst` are skipped. Directories are applied
/// last (deepest first), so read-only ones don't block their contents.
pub fn unpack(r: impl Read, dst: &Path) -> io::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(r));
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive.set_preserve_mtime(true);
    archive.set_overwrite(true);
    archive.set_unpack_xattrs(false);
    let mut dirs = Vec::new();
    let mut root = None;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let header = entry.header();
        let is_root = entry
            .path()?
            .components()
            .all(|c| matches!(c, Component::CurDir | Component::RootDir));
        if header.entry_type() == tar::EntryType::Directory {
            if is_root {
                root = Some((header.mode()?, header.uid()?, header.gid()?));
            } else {
                dirs.push(entry);
            }
            continue;
        }
        entry.unpack_in(dst)?;
    }
    dirs.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut d in dirs {
        d.unpack_in(dst)?;
    }
    if let Some((mode, uid, gid)) = root {
        use std::os::unix::fs::PermissionsExt;
        let (uid, gid) = (u32::try_from(uid).ok(), u32::try_from(gid).ok());
        std::os::unix::fs::chown(dst, uid, gid)?;
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode & 0o7777))?;
    }
    Ok(())
}

/// Remove everything inside `dir` (not `dir` itself: it is the volume's
/// mountpoint).
fn empty_dir(dir: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path: PathBuf = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("modula-volume-{name}-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_directory_round_trips() {
        let src = tmp("src");
        std::fs::create_dir_all(src.join("pg/base")).unwrap();
        std::fs::write(src.join("pg/base/1"), vec![7u8; 300_000]).unwrap();
        std::fs::write(src.join("top.txt"), b"hello").unwrap();
        std::fs::set_permissions(src.join("top.txt"), std::fs::Permissions::from_mode(0o640))
            .unwrap();
        std::os::unix::fs::symlink("pg/base/1", src.join("link")).unwrap();
        // A read-only directory must not block its own contents.
        std::fs::create_dir(src.join("ro")).unwrap();
        std::fs::write(src.join("ro/f"), b"x").unwrap();
        std::fs::set_permissions(src.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut archive = Vec::new();
        pack(&src, &mut archive).unwrap();

        let dst = tmp("dst");
        // Leftovers from an earlier, interrupted copy go away.
        std::fs::write(dst.join("stale"), b"old").unwrap();
        empty_dir(&dst).unwrap();
        unpack(&archive[..], &dst).unwrap();

        assert_eq!(std::fs::read(dst.join("top.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("pg/base/1")).unwrap().len(), 300_000);
        assert_eq!(std::fs::read(dst.join("ro/f")).unwrap(), b"x");
        assert!(!dst.join("stale").exists());
        assert_eq!(
            std::fs::read_link(dst.join("link")).unwrap(),
            PathBuf::from("pg/base/1")
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&dst.join("top.txt")), 0o640);
        assert_eq!(mode(&dst.join("ro")), 0o555);
        assert_eq!(mode(&dst), 0o700, "the volume root keeps its mode");

        for d in [&src, &dst] {
            std::fs::set_permissions(d.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn an_empty_archive_unpacks_to_nothing() {
        let mut archive = Vec::new();
        pack_empty(&mut archive).unwrap();
        let dst = tmp("empty");
        unpack(&archive[..], &dst).unwrap();
        assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 0);
        std::fs::remove_dir_all(&dst).unwrap();
    }

    #[test]
    fn only_modula_volumes() {
        assert!(valid_volume("modula-vol-0123abcdEF"));
        assert!(!valid_volume("modula-vol-"));
        assert!(!valid_volume("modula-vol-../etc"));
        assert!(!valid_volume("postgres-data"));
    }
}
