//! Source archives: download a gzipped tarball and unpack it safely.
//!
//! GitHub tarballs hold a single top-level directory (`owner-repo-sha/`),
//! which is stripped; `modula up` packs uploads the same way (`app/`).
//! Uploaded archives carry a SHA-256 that is checked before unpacking. Only regular files, directories and symlinks that stay
//! inside the tree are unpacked; anything else (hard links, devices, paths
//! with `..`) is skipped. Sizes and entry counts are capped.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use futures::StreamExt;
use modula_proto::node::v1 as pb;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const MAX_ARCHIVE: u64 = 1024 * 1024 * 1024;
const MAX_UNPACKED: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 200_000;

pub struct Fetcher {
    http: reqwest::Client,
}

impl Fetcher {
    pub fn new() -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("modula-node/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            // GitHub redirects tarball requests to codeload; keep it to https.
            .redirect(reqwest::redirect::Policy::custom(|a| {
                if a.previous().len() > 5 || a.url().scheme() != "https" {
                    a.stop()
                } else {
                    a.follow()
                }
            }))
            .build()?;
        Ok(Self { http })
    }

    /// Download `url` (sending `auth` as the Authorization header, if any) to `dest`.
    pub async fn download(&self, url: &str, auth: &str, dest: &Path) -> anyhow::Result<u64> {
        let mut req = self.http.get(url);
        if !auth.is_empty() {
            req = req.header(reqwest::header::AUTHORIZATION, auth);
        }
        let resp = req.send().await.context("cannot reach the source host")?;
        if !resp.status().is_success() {
            bail!(
                "downloading the source failed: HTTP {}",
                resp.status().as_u16()
            );
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let mut size = 0u64;
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.context("the source download was interrupted")?;
            size += chunk.len() as u64;
            if size > MAX_ARCHIVE {
                bail!(
                    "the source archive is larger than {} MB",
                    MAX_ARCHIVE / 1024 / 1024
                );
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(size)
    }
}

/// A source fetched and unpacked into a build directory.
pub struct Fetched {
    /// The unpacked tree (before `root_dir`).
    pub dir: PathBuf,
    pub files: usize,
    /// Compressed size.
    pub size: u64,
}

/// Download `spec`'s source into `dir`, check its hash (when the spec has
/// one) and unpack it into `dir/src`. The archive is deleted afterwards.
pub async fn fetch(fetcher: &Fetcher, spec: &pb::Build, dir: &Path) -> anyhow::Result<Fetched> {
    let archive = dir.join("source.tar.gz");
    let size = fetcher
        .download(&spec.source_url, &spec.source_auth, &archive)
        .await?;
    if !spec.source_sha256.is_empty() {
        let (a, want) = (archive.clone(), spec.source_sha256.clone());
        tokio::task::spawn_blocking(move || verify_sha256(&a, &want)).await??;
    }
    let src = dir.join("src");
    let files = {
        let (a, s) = (archive.clone(), src.clone());
        tokio::task::spawn_blocking(move || unpack(&a, &s)).await??
    };
    let _ = tokio::fs::remove_file(&archive).await;
    Ok(Fetched {
        dir: src,
        files,
        size,
    })
}

/// Fails unless `path` hashes to `want` (lowercase hex SHA-256).
pub fn verify_sha256(path: &Path, want: &str) -> anyhow::Result<()> {
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex::encode(hasher.finalize());
    if !got.eq_ignore_ascii_case(want) {
        bail!("the downloaded source does not match its checksum (expected {want}, got {got})");
    }
    Ok(())
}

/// Unpack `archive` (tar.gz) into `dest`, stripping the top-level directory.
/// Returns the number of files written.
pub fn unpack(archive: &Path, dest: &Path) -> anyhow::Result<usize> {
    std::fs::create_dir_all(dest)?;
    let root = dest.canonicalize()?;
    let gz = flate2::read::GzDecoder::new(std::fs::File::open(archive)?);
    let mut tar = tar::Archive::new(gz);
    let (mut files, mut total, mut entries) = (0usize, 0u64, 0usize);
    for entry in tar.entries()? {
        let mut entry = entry?;
        entries += 1;
        if entries > MAX_ENTRIES {
            bail!("the source has more than {MAX_ENTRIES} files");
        }
        let path = entry.path()?.into_owned();
        let Some(rel) = strip_top(&path) else {
            continue;
        };
        let kind = entry.header().entry_type();
        let name = rel.file_name().context("entry without a name")?.to_owned();
        // Every directory on the way is checked (and created) before the
        // entry itself: a symlink planted by an earlier entry is never
        // followed, so the lexical checks below are the physical truth.
        if kind.is_dir() {
            if let Some(parent) = prepare_parent(&root, &rel)? {
                make_dir(&parent.join(&name))?;
            }
        } else if kind.is_file() {
            total += entry.size();
            if total > MAX_UNPACKED {
                bail!(
                    "the source is larger than {} GB unpacked",
                    MAX_UNPACKED >> 30
                );
            }
            let Some(parent) = prepare_parent(&root, &rel)? else {
                continue;
            };
            let executable = entry.header().mode().is_ok_and(|m| m & 0o111 != 0);
            let out = parent.join(&name);
            if std::fs::symlink_metadata(&out).is_ok_and(|m| m.is_dir()) {
                continue; // a file over a directory: skipped
            }
            let _ = std::fs::remove_file(&out); // never write through an existing symlink
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&out)?;
            std::io::copy(&mut entry, &mut f)?;
            #[cfg(unix)]
            if executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755))?;
            }
            files += 1;
        } else if kind.is_symlink() {
            let Some(link) = entry.link_name()? else {
                continue;
            };
            if !link_stays_inside(&rel, &link) {
                continue;
            }
            let Some(parent) = prepare_parent(&root, &rel)? else {
                continue;
            };
            let out = parent.join(&name);
            if std::fs::symlink_metadata(&out).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let _ = std::fs::remove_file(&out);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &out)?;
        }
        // Hard links, devices and FIFOs are never needed to build an app: skipped.
    }
    Ok(files)
}

/// Creates the directories above `rel` inside `root` (canonical) and
/// returns the parent's path, or `None` (nothing created further) when one
/// of them already exists as anything but a real directory: a symlink, even
/// one pointing inside the tree, or a file.
fn prepare_parent(root: &Path, rel: &Path) -> anyhow::Result<Option<PathBuf>> {
    let mut dir = root.to_path_buf();
    let Some(parent) = rel.parent() else {
        return Ok(Some(dir));
    };
    for c in parent.components() {
        let Component::Normal(part) = c else {
            return Ok(None);
        };
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => make_dir(&dir)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(dir))
}

/// `create_dir` that accepts an existing entry of that name.
fn make_dir(dir: &Path) -> anyhow::Result<()> {
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(()),
        // A file or link of the same name is left as it is.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `owner-repo-sha/a/b` → `a/b`; `None` for the top directory itself or
/// anything that is not a plain relative path.
fn strip_top(path: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    match parts.next() {
        Some(Component::Normal(_)) => {}
        _ => return None,
    }
    let rest: PathBuf = parts.collect();
    let plain = !rest.as_os_str().is_empty()
        && rest.components().all(|c| matches!(c, Component::Normal(_)));
    plain.then_some(rest)
}

/// A relative symlink at `rel` pointing to `link` must resolve inside the
/// tree. Its `..`s must all come first: they climb from the link's real
/// directory (no directory above `rel` is a symlink, see
/// [`prepare_parent`]); a `..` after a name could climb out of wherever an
/// in-tree symlink of that name points (`s -> .`, then `k -> s/s/../..`).
/// Each link checked this way resolves inside, so following several does
/// too.
fn link_stays_inside(rel: &Path, link: &Path) -> bool {
    if link.is_absolute() {
        return false;
    }
    let mut depth: i64 = rel.components().count() as i64 - 1;
    let mut named = false;
    for c in link.components() {
        match c {
            Component::Normal(_) => named = true,
            Component::ParentDir if !named => depth -= 1,
            Component::CurDir => {}
            _ => return false,
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(
            strip_top(Path::new("o-r-1/a/b.txt")).unwrap(),
            Path::new("a/b.txt")
        );
        assert_eq!(strip_top(Path::new("o-r-1/")), None);
        assert_eq!(strip_top(Path::new("o-r-1/../x")), None);
        assert_eq!(strip_top(Path::new("/etc/passwd")), None);
        assert!(link_stays_inside(Path::new("a/link"), Path::new("../b")));
        assert!(!link_stays_inside(
            Path::new("a/link"),
            Path::new("../../b")
        ));
        assert!(!link_stays_inside(
            Path::new("link"),
            Path::new("/etc/passwd")
        ));
        assert!(link_stays_inside(
            Path::new("a/b/l"),
            Path::new("../../c/./d")
        ));
        // Climbing after a name can leave the tree through a symlink.
        assert!(!link_stays_inside(Path::new("k"), Path::new("s/s/s/../..")));
        assert!(!link_stays_inside(Path::new("a/k"), Path::new("b/../c")));
    }

    type Entry<'a> = (&'a str, tar::EntryType, &'a [u8], Option<&'a str>);

    /// A gzipped tarball of `entries`, names written raw (hostile ones too).
    fn tgz(dir: &Path, entries: &[Entry]) -> PathBuf {
        let path = dir.join("src.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut b = tar::Builder::new(gz);
        for &(name, kind, data, link) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            if let Some(l) = link {
                // Raw too: the builder refuses some hostile link names.
                let raw = &mut h.as_gnu_mut().unwrap().linkname;
                raw[..l.len()].copy_from_slice(l.as_bytes());
            }
            let raw = &mut h.as_old_mut().name;
            raw[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            b.append(&h, data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        path
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("modula-src-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every path under `dir`, symlinks not followed.
    fn tree(dir: &Path) -> Vec<String> {
        let mut out = vec![];
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            out.push(p.strip_prefix(dir).unwrap().display().to_string());
            if e.file_type().unwrap().is_dir() {
                out.extend(
                    tree(&p)
                        .into_iter()
                        .map(|c| format!("{}/{c}", e.file_name().display())),
                );
            }
        }
        out.sort();
        out
    }

    #[test]
    fn never_unpacks_through_a_symlink() {
        use tar::EntryType::{Directory, Regular, Symlink};
        let dir = scratch();
        let tgz = tgz(
            &dir,
            &[
                ("t/", Directory, b"", None),
                ("t/s", Symlink, b"", Some(".")),
                // Lexically inside (8 levels up from 8 levels down); really
                // /etc/passwd if created through the `s -> .` loop.
                (
                    "t/s/s/s/s/s/s/s/s/k",
                    Symlink,
                    b"",
                    Some("../../../../../../../../etc/passwd"),
                ),
                ("t/s/f", Regular, b"x", None),
                ("t/s/d/", Directory, b"", None),
                // A `..` after a name climbs out through `s`.
                ("t/j", Symlink, b"", Some("s/s/s/../../etc")),
                ("t/ok", Regular, b"y", None),
            ],
        );
        let out = dir.join("out");
        assert_eq!(unpack(&tgz, &out).unwrap(), 1);
        assert_eq!(tree(&out), ["ok", "s"]);
        assert!(out.join("s").is_symlink());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn never_creates_anything_outside() {
        use tar::EntryType::{Directory, Regular, Symlink};
        let dir = scratch();
        let (out, outside) = (dir.join("out"), dir.join("outside"));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // A link already in the destination (as an earlier entry could
        // leave one) pointing out of the tree.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, out.join("d")).unwrap();
        let tgz = tgz(
            &dir,
            &[
                ("t/d/sub/", Directory, b"", None),
                ("t/d/sub/f", Regular, b"x", None),
                ("t/d/f", Regular, b"x", None),
                ("t/d/l", Symlink, b"", Some("f")),
                ("t/e", Symlink, b"", Some("../outside")),
                ("t/e/sub/", Directory, b"", None),
            ],
        );
        assert_eq!(unpack(&tgz, &out).unwrap(), 0);
        assert!(tree(&outside).is_empty(), "{:?}", tree(&outside));
        assert!(!out.join("e").is_symlink(), "escaping link skipped");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A gzipped tarball shaped like GitHub's, plus hostile entries.
    fn archive(dir: &Path) -> PathBuf {
        let path = dir.join("src.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut b = tar::Builder::new(gz);
        let mut add = |name: &str, kind: tar::EntryType, data: &[u8], link: Option<&str>| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_size(data.len() as u64);
            h.set_mode(if name.ends_with(".sh") { 0o755 } else { 0o644 });
            if let Some(l) = link {
                h.set_link_name(l).unwrap();
            }
            // Bypass the builder's own path checks to plant hostile names.
            let raw = &mut h.as_old_mut().name;
            raw[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            b.append(&h, data).unwrap();
        };
        add("acme-app-1234/", tar::EntryType::Directory, b"", None);
        add(
            "acme-app-1234/package.json",
            tar::EntryType::Regular,
            b"{}",
            None,
        );
        add(
            "acme-app-1234/bin/run.sh",
            tar::EntryType::Regular,
            b"#!/bin/sh",
            None,
        );
        add(
            "acme-app-1234/../escape.txt",
            tar::EntryType::Regular,
            b"x",
            None,
        );
        add(
            "acme-app-1234/up",
            tar::EntryType::Symlink,
            b"",
            Some("../../.."),
        );
        add(
            "acme-app-1234/up/owned.txt",
            tar::EntryType::Regular,
            b"x",
            None,
        );
        add(
            "acme-app-1234/ok-link",
            tar::EntryType::Symlink,
            b"",
            Some("bin/run.sh"),
        );
        add(
            "acme-app-1234/hard",
            tar::EntryType::Link,
            b"",
            Some("/etc/passwd"),
        );
        b.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let dir = std::env::temp_dir().join(format!("modula-sum-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a");
        std::fs::write(&f, b"hello").unwrap();
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        verify_sha256(&f, good).unwrap();
        let e = verify_sha256(&f, &"0".repeat(64)).unwrap_err();
        assert!(e.to_string().contains("checksum"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unpacks_only_what_stays_inside() {
        let dir = std::env::temp_dir().join(format!("modula-src-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let tgz = archive(&dir);
        let out = dir.join("out");
        let files = unpack(&tgz, &out).unwrap();
        // up/owned.txt lands in a real directory: the escaping link was skipped.
        assert_eq!(files, 3);
        assert!(out.join("package.json").is_file());
        assert!(out.join("ok-link").exists());
        assert!(!out.join("up").is_symlink(), "escaping symlink skipped");
        assert!(!out.join("hard").exists(), "hard links skipped");
        assert!(!dir.join("escape.txt").exists());
        assert!(out.join("up/owned.txt").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(out.join("bin/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "executables stay executable");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
