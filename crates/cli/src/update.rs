//! Releases of the CLI and the daily "a newer version is available" notice.
//!
//! A release is checked the way scripts/install-cli.sh checks it (and the
//! server's `updates::release`, whose verification this mirrors): `SHA256SUMS`
//! must carry a valid Ed25519 signature from the release key, `VERSION` must
//! be listed in it, and a binary is downloaded by its checksum
//! (`files/<sha256>`) and hashed again before it replaces this executable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// This CLI's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_RELEASE_URL: &str = "https://downloads.usemodula.org/releases";
/// deploy/release.pub.pem, the key install-cli.sh embeds too.
const RELEASE_PUBKEY_PEM: &str = include_str!("../../../deploy/release.pub.pem");
/// DER prefix of an Ed25519 SubjectPublicKeyInfo; the raw key follows.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
const MAX_SUMS: usize = 64 * 1024;
const MAX_VERSION: usize = 128;
const MAX_BINARY: usize = 256 * 1024 * 1024;
/// How often the notice checks for new versions.
pub const CHECK_EVERY_SECS: u64 = 24 * 60 * 60;

/// Where releases are published (`MODULA_RELEASE_URL`, like install-cli.sh).
pub fn release_base() -> String {
    std::env::var("MODULA_RELEASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

/// An Ed25519 public key from base64 SubjectPublicKeyInfo (a PEM body).
pub fn pubkey_from_spki(b64: &str) -> anyhow::Result<[u8; 32]> {
    let der = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("not base64")?;
    let key = der
        .strip_prefix(&ED25519_SPKI_PREFIX[..])
        .context("not an Ed25519 public key")?;
    key.try_into().context("not an Ed25519 public key")
}

/// The release key: `MODULA_RELEASE_PUBKEY` (as install-cli.sh takes it),
/// else the one built in.
pub fn release_pubkey() -> anyhow::Result<[u8; 32]> {
    match std::env::var("MODULA_RELEASE_PUBKEY")
        .ok()
        .filter(|v| !v.is_empty())
    {
        Some(k) => pubkey_from_spki(&k).context("MODULA_RELEASE_PUBKEY"),
        None => Ok(embedded_pubkey()),
    }
}

/// deploy/release.pub.pem.
pub fn embedded_pubkey() -> [u8; 32] {
    let body: String = RELEASE_PUBKEY_PEM
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    pubkey_from_spki(&body).expect("deploy/release.pub.pem is an Ed25519 key")
}

/// One verified release.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: semver::Version,
    /// The signed `SHA256SUMS`.
    pub sums: String,
}

impl Release {
    /// The checksum of the file `name` in this release.
    pub fn sha256_of(&self, name: &str) -> Option<&str> {
        lookup(&self.sums, name)
    }
}

/// Check one release's files: `sums` must be signed by `pubkey`, and
/// `version_file` must be listed in it as `VERSION`.
pub fn verify(
    sums: &[u8],
    sig: &[u8],
    version_file: &[u8],
    pubkey: &[u8; 32],
) -> anyhow::Result<Release> {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pubkey)
        .verify(sums, sig)
        .map_err(|_| anyhow::anyhow!("SHA256SUMS is not signed by the release key"))?;
    let sums = std::str::from_utf8(sums).context("SHA256SUMS is not text")?;
    let want = hex::encode(Sha256::digest(version_file));
    if lookup(sums, "VERSION") != Some(want.as_str()) {
        bail!("VERSION does not match SHA256SUMS");
    }
    let v = std::str::from_utf8(version_file)
        .context("VERSION is not text")?
        .trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    let version =
        semver::Version::parse(v).with_context(|| format!("VERSION is not a version: {v:?}"))?;
    Ok(Release {
        version,
        sums: sums.to_owned(),
    })
}

/// The checksum listed for `name` (`<sha256>  name`, or `*name` in binary
/// mode). Only a well-formed sha256 counts.
pub fn lookup<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let sum = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file == name && sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(sum)
    })
}

/// The release binary for an OS and CPU (`std::env::consts` names).
pub fn asset_name(os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "linux" => "linux",
        "macos" => "darwin",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some(format!("modula-{os}-{arch}"))
}

/// The release binary for this machine.
pub fn this_asset() -> Option<String> {
    asset_name(std::env::consts::OS, std::env::consts::ARCH)
}

/// `candidate` is a newer version than `current`; one that doesn't parse
/// never is.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (
        semver::Version::parse(candidate.trim_start_matches('v')),
        semver::Version::parse(current.trim_start_matches('v')),
    ) {
        (Ok(c), Ok(cur)) => c > cur,
        _ => false,
    }
}

/// Where a release's files (and notes, when published) live.
pub fn release_page(base: &str, version: &str) -> String {
    format!("{}/download/v{version}/", base.trim_end_matches('/'))
}

pub fn http_client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    crate::api::install_crypto();
    reqwest::Client::builder()
        .user_agent(crate::api::USER_AGENT)
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .timeout(timeout)
        .build()
        .context("cannot set up the HTTP client")
}

/// The newest published release, verified.
pub async fn fetch_latest(
    http: &reqwest::Client,
    base: &str,
    pubkey: &[u8; 32],
) -> anyhow::Result<Release> {
    let url = |file: &str| format!("{base}/latest/download/{file}");
    let version = get(http, &url("VERSION"), MAX_VERSION).await?;
    let sums = get(http, &url("SHA256SUMS"), MAX_SUMS).await?;
    let sig = get(http, &url("SHA256SUMS.sig"), 1024).await?;
    verify(&sums, &sig, &version, pubkey)
}

/// Download `files/<sha256>` and check it hashes to that.
pub async fn download(http: &reqwest::Client, base: &str, sha256: &str) -> anyhow::Result<Vec<u8>> {
    let bytes = get(http, &format!("{base}/files/{sha256}"), MAX_BINARY).await?;
    if hex::encode(Sha256::digest(&bytes)) != sha256 {
        bail!("the download doesn't match the signed SHA256SUMS; nothing was installed");
    }
    Ok(bytes)
}

async fn get(http: &reqwest::Client, url: &str, max: usize) -> anyhow::Result<Vec<u8>> {
    let resp = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("cannot reach {url}"))?;
    if !resp.status().is_success() {
        bail!("{url} answered {}", resp.status());
    }
    if resp.content_length().is_some_and(|l| l > max as u64) {
        bail!("{url} is larger than expected");
    }
    let body = resp.bytes().await.context("download interrupted")?;
    if body.len() > max {
        bail!("{url} is larger than expected");
    }
    Ok(body.to_vec())
}

/// The file the running `modula` was started from (symlinks resolved, so a
/// link in ~/bin keeps pointing at the replaced file).
pub fn current_exe() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot find the running executable")?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

/// Atomically replace `target` with `bytes`: written next to it, made
/// executable, renamed over it.
pub fn replace_file(target: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let dir = target.parent().context("the executable has no directory")?;
    let tmp = dir.join(format!(".modula-update-{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, target)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            bail!(
                "no permission to replace {} ({e}). Run `sudo modula update`, or reinstall: \
                 curl -fsSL https://usemodula.org/install-cli.sh | bash",
                target.display()
            );
        }
        return Err(e).with_context(|| format!("cannot replace {}", target.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------- the notice

/// What the last checks found, in `update-check.toml` next to the config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckCache {
    /// Unix seconds of the last CLI release check.
    #[serde(default)]
    pub cli_checked_at: Option<u64>,
    /// The newest CLI release then.
    #[serde(default)]
    pub cli_latest: Option<String>,
    /// Unix seconds of the last background check started (finished or not).
    #[serde(default)]
    pub attempted_at: Option<u64>,
    /// Per instance URL.
    #[serde(default)]
    pub servers: BTreeMap<String, ServerCheck>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCheck {
    pub checked_at: u64,
    /// The version it can update to, when there is one.
    #[serde(default)]
    pub available: Option<String>,
    #[serde(default)]
    pub current: Option<String>,
}

/// A check last made at `last` is due again at `now` (unix seconds): after a
/// day, or when the clock went backwards.
pub fn due(last: Option<u64>, now: u64) -> bool {
    match last {
        None => true,
        Some(t) => now < t || now - t >= CHECK_EVERY_SECS,
    }
}

pub fn cache_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("update-check.toml")
}

pub fn load_cache(path: &Path) -> CheckCache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_cache(path: &Path, cache: &CheckCache) {
    let _ = crate::config::save(path, cache);
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The notice lines for what `cache` knows (for the instance `server`, when
/// logged in).
pub fn notices(cache: &CheckCache, server: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(v) = cache.cli_latest.as_deref().filter(|v| is_newer(v, VERSION)) {
        lines.push(format!(
            "modula {v} is available (you have {VERSION}): run `modula update`"
        ));
    }
    if let Some((url, s)) = server.and_then(|u| cache.servers.get(u).map(|s| (u, s)))
        && let Some(v) = s.available.as_deref()
    {
        let from = s
            .current
            .as_deref()
            .map(|c| format!(" (it runs {c})"))
            .unwrap_or_default();
        lines.push(format!(
            "Modula Deploy {v} is available for {url}{from}: run `modula server update`"
        ));
    }
    lines
}

/// The `GET /api/instance/updates` fields the notice needs.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerStatus {
    current_version: String,
    latest_version: Option<String>,
    update_available: bool,
}

/// The hidden command the notice runs in the background.
pub const CHECK_COMMAND: &str = "__update-check";

/// The once-a-day notice around a command. It never touches the network
/// itself: it prints what the last check found (in the cache), and when that
/// is a day old, starts a detached `modula __update-check` that refreshes the
/// cache for next time, so no command ever waits on it.
pub struct Notifier {
    lines: Vec<String>,
}

impl Notifier {
    /// `None` when the notice is off: not a person at a terminal, CI,
    /// `MODULA_NO_UPDATE_CHECK`, or --json.
    pub fn start(ctx: &crate::context::Ctx) -> Option<Self> {
        use std::io::IsTerminal;
        let off = std::env::var_os("MODULA_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty())
            || std::env::var_os("CI").is_some()
            || ctx.global.json
            || ctx.out.is_captured()
            || !ctx.session.interactive
            || !std::io::stderr().is_terminal()
            || !std::io::stdout().is_terminal();
        if off {
            return None;
        }
        let path = cache_path(&ctx.session.config_path);
        let mut cache = load_cache(&path);
        let server = server_url(ctx);
        let now = now();
        let due_now = due(cache.cli_checked_at, now)
            || server
                .as_ref()
                .is_some_and(|u| due(cache.servers.get(u).map(|s| s.checked_at), now));
        // A failed check (offline) is retried after an hour, not every command.
        let retry = cache
            .attempted_at
            .is_none_or(|t| now < t || now - t >= RETRY_SECS);
        if due_now && retry {
            cache.attempted_at = Some(now);
            save_cache(&path, &cache);
            spawn_check(ctx);
        }
        Some(Self {
            lines: notices(&cache, server.as_deref()),
        })
    }

    /// Print the notice (dim, on stderr).
    pub fn finish(self, out: &crate::output::Output) {
        for line in self.lines {
            eprintln!("{}", out.dim(&line));
        }
    }
}

/// How soon a check that didn't finish is tried again.
const RETRY_SECS: u64 = 60 * 60;

/// The instance the command talks to, when logged in.
fn active_api_login(ctx: &crate::context::Ctx) -> Option<crate::config::Login> {
    let active = ctx.active_login().ok().flatten()?;
    crate::config::login(
        Some(&active.saved),
        ctx.session.env_url.clone(),
        ctx.session.env_token.clone(),
    )
    .ok()
}

fn server_url(ctx: &crate::context::Ctx) -> Option<String> {
    active_api_login(ctx).map(|l| l.url.to_string())
}

/// Start `modula __update-check` detached (same directory and login), its
/// output discarded.
fn spawn_check(ctx: &crate::context::Ctx) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    if let Some(login) = &ctx.global.login {
        cmd.args(["--login", login]);
    }
    cmd.arg(CHECK_COMMAND)
        .current_dir(&ctx.session.cwd)
        .env("MODULA_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        // Its own process group: Ctrl-C on this command doesn't reach it.
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let _ = cmd.spawn();
}

/// `modula __update-check`: refresh the cache (the CLI's newest release and,
/// when logged in, the instance's available update). Errors are ignored; the
/// next check is tomorrow (or in an hour, when this one failed).
pub async fn background_check(ctx: &crate::context::Ctx) {
    let budget = Duration::from_secs(15);
    let path = cache_path(&ctx.session.config_path);
    let login = active_api_login(ctx);
    let now = now();
    let cache = load_cache(&path);
    let cli_due = due(cache.cli_checked_at, now);
    let server_due = login
        .as_ref()
        .is_some_and(|l| due(cache.servers.get(l.url.as_str()).map(|s| s.checked_at), now));
    let cli = async {
        if !cli_due {
            return None;
        }
        let http = http_client(budget).ok()?;
        let key = release_pubkey().ok()?;
        let r = fetch_latest(&http, &release_base(), &key).await.ok()?;
        Some(r.version.to_string())
    };
    let server = async {
        let login = login.filter(|_| server_due)?;
        let url = login.url.to_string();
        let api = crate::api::Api::new(login).ok()?;
        let check = match api.get::<ServerStatus>("/api/instance/updates").await {
            Ok(s) => ServerCheck {
                checked_at: self::now(),
                available: s.latest_version.filter(|_| s.update_available),
                current: Some(s.current_version),
            },
            // Not an instance admin, or an instance without the Updates API:
            // nothing to say today.
            Err(e) if crate::api::api_error(&e).is_some_and(|a| matches!(a.status, 403 | 404)) => {
                ServerCheck {
                    checked_at: self::now(),
                    ..ServerCheck::default()
                }
            }
            Err(_) => return None,
        };
        Some((url, check))
    };
    let (cli, server) = tokio::time::timeout(budget, async { tokio::join!(cli, server) })
        .await
        .unwrap_or_default();
    // Re-read: another command may have written meanwhile.
    let mut cache = load_cache(&path);
    if let Some(v) = cli {
        cache.cli_latest = Some(v);
        cache.cli_checked_at = Some(self::now());
    }
    if let Some((url, s)) = server {
        cache.servers.insert(url, s);
    }
    save_cache(&path, &cache);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    fn test_key() -> (ring::signature::Ed25519KeyPair, [u8; 32]) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = kp.public_key().as_ref().try_into().unwrap();
        (kp, public)
    }

    /// SHA256SUMS listing `VERSION` and a binary (binary mode, `*name`).
    fn signed_release(
        key: &ring::signature::Ed25519KeyPair,
        version: &str,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let version_file = format!("{version}\n").into_bytes();
        let sums = format!(
            "{} *modula-linux-arm64\n{}  VERSION\n",
            hex::encode(Sha256::digest(b"binary")),
            hex::encode(Sha256::digest(&version_file))
        )
        .into_bytes();
        let sig = key.sign(&sums).as_ref().to_vec();
        (sums, sig, version_file)
    }

    #[test]
    fn the_embedded_key_matches_install_cli_sh() {
        let k = embedded_pubkey();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .encode([&ED25519_SPKI_PREFIX[..], &k].concat()),
            "MCowBQYDK2VwAyEAVl1B0m1qDZY+gGHSLVry769Tl1KmGKKFhFiav9ccXCE="
        );
        let script = include_str!("../../../scripts/install-cli.sh");
        assert!(script.contains("MCowBQYDK2VwAyEAVl1B0m1qDZY+gGHSLVry769Tl1KmGKKFhFiav9ccXCE="));
        assert!(pubkey_from_spki("bm90IGEga2V5").is_err());
    }

    #[test]
    fn verifies_the_release_and_finds_binaries() {
        let (key, public) = test_key();
        let (sums, sig, version) = signed_release(&key, "0.3.1");
        let r = verify(&sums, &sig, &version, &public).unwrap();
        assert_eq!(r.version.to_string(), "0.3.1");
        assert_eq!(
            r.sha256_of("modula-linux-arm64"),
            Some(hex::encode(Sha256::digest(b"binary")).as_str())
        );
        assert_eq!(r.sha256_of("modula-darwin-arm64"), None);

        let (_, other) = test_key();
        assert!(verify(&sums, &sig, &version, &other).is_err());
        let mut tampered = sums.clone();
        tampered[0] ^= 1;
        assert!(verify(&tampered, &sig, &version, &public).is_err());
        assert!(verify(&sums, &sig, b"9.9.9\n", &public).is_err());
        let (sums, sig, version) = signed_release(&key, "latest");
        assert!(verify(&sums, &sig, &version, &public).is_err());
    }

    #[test]
    fn sums_lookup_wants_a_real_checksum() {
        let sum = "a".repeat(64);
        let sums = format!("{sum}  modula-darwin-arm64\nnothex  modula-linux-amd64\n\n{sum}\n");
        assert_eq!(lookup(&sums, "modula-darwin-arm64"), Some(sum.as_str()));
        assert_eq!(lookup(&sums, "modula-linux-amd64"), None);
        assert_eq!(lookup(&sums, "modula-darwin"), None);
    }

    #[test]
    fn asset_names() {
        assert_eq!(
            asset_name("macos", "aarch64").as_deref(),
            Some("modula-darwin-arm64")
        );
        assert_eq!(
            asset_name("linux", "x86_64").as_deref(),
            Some("modula-linux-amd64")
        );
        assert_eq!(asset_name("windows", "x86_64"), None);
        assert_eq!(asset_name("linux", "riscv64"), None);
    }

    #[test]
    fn version_order() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("v1.0.0", "1.0.0-rc.1"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert_eq!(
            release_page("https://downloads.example.com/releases/", "0.2.0"),
            "https://downloads.example.com/releases/download/v0.2.0/"
        );
    }

    #[test]
    fn checks_once_a_day() {
        let t = 1_000_000;
        assert!(due(None, t));
        assert!(!due(Some(t), t));
        assert!(!due(Some(t), t + CHECK_EVERY_SECS - 1));
        assert!(due(Some(t), t + CHECK_EVERY_SECS));
        // The clock went backwards: check again.
        assert!(due(Some(t), t - 1));
    }

    #[test]
    fn notices_only_for_newer_versions() {
        let url = "https://deploy.example.com/";
        let mut cache = CheckCache {
            cli_latest: Some(VERSION.into()),
            ..Default::default()
        };
        assert!(notices(&cache, Some(url)).is_empty());
        cache.cli_latest = Some("999.0.0".into());
        cache.servers.insert(
            url.into(),
            ServerCheck {
                checked_at: 1,
                available: Some("999.0.1".into()),
                current: Some("0.1.0".into()),
            },
        );
        let lines = notices(&cache, Some(url));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("modula 999.0.0 is available"));
        assert!(lines[1].contains("999.0.1") && lines[1].contains("modula server update"));
        // Another instance (or none) gets only the CLI line.
        assert_eq!(notices(&cache, Some("https://other/")).len(), 1);
        assert_eq!(notices(&cache, None).len(), 1);
    }

    #[test]
    fn cache_round_trips_and_tolerates_garbage() {
        let dir = std::env::temp_dir().join(format!(
            "modula-update-test-{}-{}",
            std::process::id(),
            now()
        ));
        let path = cache_path(&dir.join("config.toml"));
        assert_eq!(path, dir.join("update-check.toml"));
        assert_eq!(load_cache(&path), CheckCache::default());
        let mut cache = CheckCache {
            cli_checked_at: Some(5),
            cli_latest: Some("0.2.0".into()),
            ..Default::default()
        };
        cache
            .servers
            .insert("https://a/".into(), ServerCheck::default());
        save_cache(&path, &cache);
        assert_eq!(load_cache(&path), cache);
        std::fs::write(&path, "not = [toml").unwrap();
        assert_eq!(load_cache(&path), CheckCache::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replaces_a_file_in_place() {
        let dir = std::env::temp_dir().join(format!(
            "modula-replace-test-{}-{}",
            std::process::id(),
            now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("modula");
        std::fs::write(&target, b"old").unwrap();
        replace_file(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        // Nothing left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
