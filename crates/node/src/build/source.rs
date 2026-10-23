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
        let target = root.join(&rel);
        if kind.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if kind.is_file() {
            total += entry.size();
            if total > MAX_UNPACKED {
                bail!(
                    "the source is larger than {} GB unpacked",
                    MAX_UNPACKED >> 30
                );
            }
            let Some(parent) = inside(&root, &target)? else {
                continue;
            };
            let executable = entry.header().mode().is_ok_and(|m| m & 0o111 != 0);
            let out = parent.join(target.file_name().context("file without a name")?);
            let _ = std::fs::remove_file(&out); // never write through an existing symlink
            let mut f = std::fs::File::create(&out)?;
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
            let Some(parent) = inside(&root, &target)? else {
                continue;
            };
            let out = parent.join(target.file_name().context("link without a name")?);
            let _ = std::fs::remove_file(&out);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &out)?;
        }
        // Hard links, devices and FIFOs are never needed to build an app: skipped.
    }
    Ok(files)
}

/// Creates `target`'s parent and returns its real path, or `None` if it
/// resolves outside `root` (through a symlink planted by an earlier entry).
fn inside(root: &Path, target: &Path) -> anyhow::Result<Option<PathBuf>> {
    let parent = target.parent().context("path without a parent")?;
    std::fs::create_dir_all(parent)?;
    let real = parent.canonicalize()?;
    Ok(real.starts_with(root).then_some(real))
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

/// A relative symlink at `rel` pointing to `link` must resolve inside the tree.
fn link_stays_inside(rel: &Path, link: &Path) -> bool {
    if link.is_absolute() {
        return false;
    }
    let mut depth: i64 = rel.components().count() as i64 - 1;
    for c in link.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => depth -= 1,
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
