//! `up`: deploy local files without GitHub (Railway's `railway up`).
//!
//! The directory is packed into a gzipped tarball under one top-level `app/`
//! directory (the shape the node unpacks, like a GitHub tarball), honoring
//! `.gitignore`, `.git/info/exclude`, the global Git excludes and
//! `.modulaignore` (same syntax). `.git/` and `.modula/` are never included,
//! nor is this CLI's own config file. The archive is uploaded with its
//! SHA-256, then deployed: the service builds it with its usual settings
//! (Railpack or Dockerfile, root directory, config file) without changing
//! the service's configured source.
//!
//! With no app service to deploy to, `up` creates one (source: local files,
//! the Dockerfile builder when the directory has a Dockerfile), named after
//! `--service` or the directory, and links this directory to it.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::api::{LocalGit, UploadDeployment, UploadRef};
use crate::cli::UpArgs;
use crate::commands::{deploy, logs};
use crate::context::Ctx;
use crate::output::usage;

/// Refuse to upload more than this (compressed). The server has its own
/// limit (200 MB by default) and says so if it is lower.
pub const MAX_ARCHIVE: u64 = 1024 * 1024 * 1024;
/// More files than this is almost certainly a missing ignore rule.
const MAX_FILES: usize = 200_000;
/// The archive's top-level directory.
const TOP: &str = "app";
/// Ignore file read in every directory, in addition to `.gitignore`.
pub const IGNORE_FILE: &str = ".modulaignore";

/// What [`pack`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    pub files: usize,
    /// Symlinks left out because they point outside the directory.
    pub skipped_links: usize,
    /// Compressed size.
    pub size: u64,
    pub sha256: String,
}

pub async fn run(ctx: &mut Ctx, args: UpArgs) -> anyhow::Result<i32> {
    let dir = match &args.path {
        Some(p) => ctx.session.cwd.join(p),
        None => ctx.session.cwd.clone(),
    };
    if !dir.is_dir() {
        return Err(usage(format!("{} is not a directory", dir.display())));
    }
    let dir = dir.canonicalize()?;
    let t = target(ctx, &dir).await?;

    ctx.out.note(format!("Packing {}…", dir.display()));
    let archive = std::env::temp_dir().join(format!(
        "modula-up-{}-{}.tar.gz",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let exclude = vec![ctx.session.config_path.clone()];
    let packed = {
        let (d, a) = (dir.clone(), archive.clone());
        tokio::task::spawn_blocking(move || pack(&d, &a, &exclude, MAX_ARCHIVE)).await?
    };
    let result = upload_and_deploy(ctx, &t, &dir, &archive, packed, &args).await;
    let _ = std::fs::remove_file(&archive);
    let d = result?;

    if args.detach {
        ctx.out.either(&d, |o| {
            o.line(format!("Started deployment {} of {}", d.id, t.label()))
        })?;
        return Ok(0);
    }
    // Build logs are for people (and CI); with --json only the result prints.
    let d = deploy::wait(ctx, &d.id, !ctx.out.json).await?;
    let code = deploy::outcome(ctx, &d);
    // In CI the job log is all there is: show why a deployment that started
    // didn't stay up.
    if args.ci && code != 0 && !ctx.out.json && d.status == "crashed" {
        ctx.out.note("Last runtime logs:");
        let opts = crate::api::endpoints::LogOptions {
            tail: 100,
            follow: false,
            build: false,
        };
        if let Err(e) = logs::print_stream(ctx, &d.id, opts).await {
            ctx.out.note(format!("  (logs unavailable: {e:#})"));
        }
    }
    ctx.out
        .either(&d, |o| o.line(format!("{} {}", d.id, o.status(&d.status))))?;
    Ok(code)
}

/// The service to deploy to: the named or linked one, else the project's
/// only app service (never a database). Creates a local-files service when
/// the named one doesn't exist or the project has no app service yet.
async fn target(ctx: &mut Ctx, dir: &Path) -> anyhow::Result<crate::context::Target> {
    let project = ctx.project().await?;
    let wanted = ctx.wanted_service(&project).map(str::to_owned);
    let existing = match wanted.as_deref() {
        Some(w) => project
            .services
            .iter()
            .find(|s| s.id == w || s.name.eq_ignore_ascii_case(w))
            .cloned(),
        None => {
            let apps: Vec<_> = project
                .services
                .iter()
                .filter(|s| s.template.is_none())
                .collect();
            match apps[..] {
                [only] => Some(only.clone()),
                [] => None,
                _ if crate::resolve::interactive() => Some(
                    (*crate::resolve::choose("Deploy to which service?", &apps, |s| {
                        crate::resolve::service_label(s)
                    })?)
                    .clone(),
                ),
                _ => {
                    return Err(usage(format!(
                        "pass --service or run `modula service <name>` (app services: {})",
                        apps.iter()
                            .map(|s| s.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            }
        }
    };
    if let Some(service) = existing {
        if service.template.is_some() {
            return Err(usage(format!(
                "{} is a database; `up` deploys app code. Pass --service <name> to create an app service",
                service.name
            )));
        }
        return ctx.target_with(project, service);
    }
    // A wanted name that is an id of nothing can't be a new service's name.
    let name = match wanted.as_deref() {
        Some(w) if crate::commands::service::service_name(w) == w => w.to_owned(),
        Some(w) => return Err(usage(format!("no service named {w}"))),
        None => crate::commands::service::service_name(
            dir.file_name().and_then(|n| n.to_str()).unwrap_or("app"),
        ),
    };
    let dockerfile = dir.join("Dockerfile").is_file();
    let config = serde_json::json!({
        "source": { "type": "local" },
        "builder": if dockerfile { "dockerfile" } else { "railpack" },
    });
    let api = ctx.api()?;
    api.create_service(
        &project.id,
        &crate::api::NewService {
            name: name.clone(),
            template: None,
            config: Some(config),
        },
    )
    .await
    .with_context(|| format!("creating the service {name}"))?;
    ctx.out.note(format!(
        "Created the service {name} in {} ({})",
        project.name,
        if dockerfile {
            "builds with your Dockerfile"
        } else {
            "Railpack detects how to build it"
        }
    ));
    let project = ctx.project().await?;
    let service = project
        .services
        .iter()
        .find(|s| s.name == name)
        .cloned()
        .context("the new service isn't listed")?;
    if let Some(mut link) = ctx.linked().filter(|l| l.project == project.id).cloned() {
        link.service = Some(service.id.clone());
        link.service_name = Some(service.name.clone());
        ctx.save_link(link)?;
    }
    ctx.target_with(project, service)
}

async fn upload_and_deploy(
    ctx: &Ctx,
    t: &crate::context::Target,
    dir: &Path,
    archive: &Path,
    packed: anyhow::Result<Packed>,
    args: &UpArgs,
) -> anyhow::Result<crate::api::Deployment> {
    let packed = packed?;
    if packed.files == 0 {
        bail!(
            "nothing to upload: every file in {} is ignored",
            dir.display()
        );
    }
    ctx.out.note(format!(
        "Packed {} files ({}){}",
        packed.files,
        megabytes(packed.size),
        match packed.skipped_links {
            0 => String::new(),
            n => format!("; skipped {n} symlinks pointing outside the directory"),
        }
    ));
    let api = ctx.api()?;
    ctx.out.note(format!("Uploading to {}…", t.label()));
    let body = progress_body(ctx, archive, packed.size).await?;
    let upload = api
        .upload_source(&t.instance, packed.size, &packed.sha256, body)
        .await
        .context("the upload failed")?;
    let git = {
        let d = dir.to_owned();
        tokio::task::spawn_blocking(move || local_git(&d)).await?
    };
    let req = UploadDeployment {
        source: UploadRef { upload: upload.id },
        message: args.message.clone().filter(|m| !m.trim().is_empty()),
        git,
    };
    let d = api.deploy_upload(&t.instance, &req).await?;
    ctx.out
        .note(format!("Deploying {} (deployment {})", t.label(), d.id));
    Ok(d)
}

/// The archive as a streamed request body that notes progress every quarter
/// (for uploads big enough to take a while).
async fn progress_body(ctx: &Ctx, archive: &Path, size: u64) -> anyhow::Result<reqwest::Body> {
    let file = tokio::fs::File::open(archive).await?;
    let out = ctx.out.clone();
    let noisy = size > 4 * 1024 * 1024;
    let stream =
        futures::stream::unfold((file, 0u64, 0u64), move |(mut file, sent, mut reported)| {
            let out = out.clone();
            async move {
                let mut buf = vec![0u8; 256 * 1024];
                match file.read(&mut buf).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buf.truncate(n);
                        let sent = sent + n as u64;
                        let quarter = sent * 4 / size.max(1);
                        if noisy && quarter > reported {
                            reported = quarter;
                            out.note(format!("  {}% of {}", quarter * 25, megabytes(size)));
                        }
                        Some((Ok::<_, std::io::Error>(buf), (file, sent, reported)))
                    }
                    Err(e) => Some((Err(e), (file, sent, u64::MAX))),
                }
            }
        });
    Ok(reqwest::Body::wrap_stream(stream.boxed()))
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
}

/// Writes through to `inner`, hashing and counting, and fails past `max`.
struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    size: u64,
    max: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.size += n as u64;
        if self.size > self.max {
            return Err(std::io::Error::other(format!(
                "the archive is larger than {}; add a {IGNORE_FILE} to leave files out",
                megabytes(self.max)
            )));
        }
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Pack `dir` into the gzipped tarball `out` (see the module docs for what
/// is left out). `exclude` are files never to include (the CLI's config).
pub fn pack(dir: &Path, out: &Path, exclude: &[PathBuf], max: u64) -> anyhow::Result<Packed> {
    let root = dir.canonicalize()?;
    let exclude: Vec<PathBuf> = exclude
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    let file =
        std::fs::File::create(out).with_context(|| format!("cannot create {}", out.display()))?;
    let hashing = Hashing {
        inner: std::io::BufWriter::new(file),
        hasher: Sha256::new(),
        size: 0,
        max,
    };
    let gz = flate2::write::GzEncoder::new(hashing, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    // Same bytes for the same files: no owners or timestamps.
    tar.mode(tar::HeaderMode::Deterministic);
    tar.follow_symlinks(false);

    let walk = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .follow_links(false)
        .add_custom_ignore_filename(IGNORE_FILE)
        .filter_entry(|e| {
            let name = e.file_name();
            !(e.file_type().is_some_and(|t| t.is_dir()) && (name == ".git" || name == ".modula"))
        })
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();
    let (mut files, mut skipped_links) = (0usize, 0usize);
    for entry in walk {
        let entry = entry?;
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(&root) else {
            continue;
        };
        if rel.as_os_str().is_empty() || exclude.iter().any(|x| x == path) {
            continue;
        }
        let name = Path::new(TOP).join(rel);
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            let target = std::fs::read_link(path)?;
            if !link_stays_inside(rel, &target) {
                skipped_links += 1;
                continue;
            }
            tar.append_path_with_name(path, &name)?;
        } else if kind.is_dir() {
            tar.append_dir(&name, path)?;
        } else if kind.is_file() {
            files += 1;
            if files > MAX_FILES {
                bail!(
                    "more than {MAX_FILES} files to upload; add a {IGNORE_FILE} to leave some out"
                );
            }
            tar.append_path_with_name(path, &name)
                .with_context(|| format!("cannot read {}", path.display()))?;
        }
    }
    let gz = tar.into_inner()?;
    let mut hashing = gz.finish()?;
    hashing.flush()?;
    Ok(Packed {
        files,
        skipped_links,
        size: hashing.size,
        sha256: hex::encode(hashing.hasher.finalize()),
    })
}

/// A relative symlink at `rel` pointing to `link` must resolve inside the tree
/// (the node skips any other kind, so they are left out here already).
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

/// The checkout `dir` is in, for display: HEAD, branch, subject and whether
/// there are uncommitted changes. `None` outside Git (or without git).
pub fn local_git(dir: &Path) -> Option<LocalGit> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "--verify", "-q", "HEAD"])?;
    let commit = (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| commit.to_ascii_lowercase());
    let branch = git(&["symbolic-ref", "--short", "-q", "HEAD"]).filter(|b| !b.is_empty());
    let subject = git(&["log", "-1", "--format=%s"]).filter(|s| !s.is_empty());
    let dirty = git(&["status", "--porcelain"]).map(|s| !s.is_empty());
    Some(LocalGit {
        commit,
        branch,
        subject,
        dirty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("modula-up-test-{name}-{nanos}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn names(archive: &Path) -> Vec<String> {
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(archive).unwrap());
        let mut tar = tar::Archive::new(gz);
        let mut out: Vec<String> = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn packing_honors_ignore_files() {
        let dir = tmp("src");
        write(&dir, "package.json", "{}");
        write(&dir, "src/index.js", "console.log(1)");
        write(&dir, ".env.example", "A=1");
        write(&dir, ".gitignore", "node_modules/\n*.log\n");
        write(&dir, "node_modules/x/index.js", "x");
        write(&dir, "debug.log", "x");
        write(&dir, ".modulaignore", "secrets/\n");
        write(&dir, "secrets/key.pem", "x");
        write(&dir, "sub/.gitignore", "local.txt\n");
        write(&dir, "sub/local.txt", "x");
        write(&dir, "sub/kept.txt", "x");
        write(&dir, ".git/config", "x");
        write(&dir, ".modula/project.toml", "x");
        write(&dir, "cli-config.toml", "token");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("src/index.js", dir.join("main.js")).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", dir.join("passwd")).unwrap();
        }

        let out = tmp("out").join("a.tar.gz");
        let packed = pack(&dir, &out, &[dir.join("cli-config.toml")], MAX_ARCHIVE).unwrap();
        let got = names(&out);
        for want in [
            "app/package.json",
            "app/src/index.js",
            "app/.env.example",
            "app/.gitignore",
            "app/.modulaignore",
            "app/sub/kept.txt",
        ] {
            assert!(got.iter().any(|g| g == want), "{want} missing from {got:?}");
        }
        for gone in [
            "node_modules",
            "debug.log",
            "secrets",
            "local.txt",
            ".git/",
            ".modula/",
            "cli-config.toml",
            "passwd",
        ] {
            assert!(!got.iter().any(|g| g.contains(gone)), "{gone} in {got:?}");
        }
        #[cfg(unix)]
        {
            assert!(got.iter().any(|g| g == "app/main.js"), "{got:?}");
            assert_eq!(packed.skipped_links, 1);
        }
        assert_eq!(packed.files, 7, "{got:?}");
        assert!(got.iter().all(|g| g.starts_with("app/")), "{got:?}");

        // Deterministic: the same files hash the same.
        let again = pack(&dir, &out, &[dir.join("cli-config.toml")], MAX_ARCHIVE).unwrap();
        assert_eq!(again.sha256, packed.sha256);
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(hex::encode(Sha256::digest(&bytes)), packed.sha256);

        // Over the size limit: refused.
        let e = pack(&dir, &out, &[], 64).unwrap_err();
        assert!(format!("{e:#}").contains("larger than"), "{e:#}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn links_must_stay_inside() {
        assert!(link_stays_inside(Path::new("a/l"), Path::new("../b")));
        assert!(!link_stays_inside(Path::new("a/l"), Path::new("../../b")));
        assert!(!link_stays_inside(Path::new("l"), Path::new("/etc/passwd")));
    }
}
