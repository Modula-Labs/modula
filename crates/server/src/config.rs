use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use modula_crypto::envelope::MasterKey;
use url::Url;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    /// The only origin the dashboard is served from. Used for CSRF Origin checks
    /// and to build every absolute link (never derived from request headers).
    pub public_url: Url,
    pub listen: SocketAddr,
    pub master_key_file: PathBuf,
    /// Peers whose `X-Forwarded-For` is trusted (the local Caddy).
    pub trusted_proxies: Vec<IpAddr>,
    /// gRPC listener for nodes (mTLS). Never the browser API port.
    pub node_listen: SocketAddr,
    /// The address nodes dial, embedded in join commands and the server cert.
    pub node_url: Url,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let get = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set"));
        let public_url: Url = get("MODULA_PUBLIC_URL")?
            .parse()
            .context("MODULA_PUBLIC_URL is not a valid URL")?;
        let trusted_proxies = std::env::var("MODULA_TRUSTED_PROXIES")
            .unwrap_or_else(|_| "127.0.0.1,::1".into())
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.trim()
                    .parse()
                    .context("invalid IP in MODULA_TRUSTED_PROXIES")
            })
            .collect::<anyhow::Result<_>>()?;
        let cfg = Self {
            database_url: get("MODULA_DATABASE_URL")?,
            redis_url: get("MODULA_REDIS_URL")?,
            public_url,
            listen: std::env::var("MODULA_LISTEN")
                .unwrap_or_else(|_| "127.0.0.1:7100".into())
                .parse()
                .context("invalid MODULA_LISTEN")?,
            master_key_file: get("MODULA_MASTER_KEY_FILE")?.into(),
            trusted_proxies,
            node_listen: std::env::var("MODULA_NODE_LISTEN")
                .unwrap_or_else(|_| "127.0.0.1:7101".into())
                .parse()
                .context("invalid MODULA_NODE_LISTEN")?,
            node_url: Url::parse("https://placeholder.invalid").expect("static url"),
        };
        let mut cfg = cfg;
        cfg.node_url = match std::env::var("MODULA_NODE_URL") {
            Ok(u) => u.parse().context("invalid MODULA_NODE_URL")?,
            Err(_) => default_node_url(&cfg.public_url, cfg.node_listen.port())?,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self.public_url.scheme() {
            "https" => {}
            "http" if self.is_local_dev() => {}
            "http" => {
                bail!("MODULA_PUBLIC_URL must be https (plain http is only allowed for localhost)")
            }
            s => bail!("unsupported MODULA_PUBLIC_URL scheme {s}"),
        }
        if self.public_url.host_str().is_none() {
            bail!("MODULA_PUBLIC_URL must have a host");
        }
        Ok(())
    }

    pub fn is_local_dev(&self) -> bool {
        matches!(
            self.public_url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        )
    }

    /// `__Host-` cookies need Secure, which needs https.
    pub fn secure_cookies(&self) -> bool {
        self.public_url.scheme() == "https"
    }

    /// Exact `Origin` header value browsers send for the dashboard.
    pub fn public_origin(&self) -> String {
        self.public_url.origin().ascii_serialization()
    }

    /// Names the node listener's certificate is valid for.
    pub fn node_hosts(&self) -> Vec<String> {
        let mut hosts = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        if let Some(h) = self.node_url.host_str() {
            let h = h.trim_start_matches('[').trim_end_matches(']').to_owned();
            if !hosts.contains(&h) {
                hosts.insert(0, h);
            }
        }
        hosts
    }

    pub fn load_master_key(&self) -> anyhow::Result<MasterKey> {
        load_master_key(&self.master_key_file)
    }
}

fn default_node_url(public: &Url, port: u16) -> anyhow::Result<Url> {
    let host = public.host_str().context("MODULA_PUBLIC_URL has no host")?;
    Ok(format!("https://{host}:{port}").parse()?)
}

pub fn load_master_key(path: &Path) -> anyhow::Result<MasterKey> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("cannot stat master key {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "master key {} is accessible by group/others (mode {:o}); run chmod 600",
                path.display(),
                mode & 0o777
            );
        }
    }
    let raw = zeroize::Zeroizing::new(
        std::fs::read_to_string(path)
            .with_context(|| format!("cannot read master key {}", path.display()))?,
    );
    MasterKey::from_encoded(&raw).context("master key file is malformed")
}

/// Write a fresh master key with mode 0600. Refuses to overwrite.
pub fn write_new_master_key(path: &Path) -> anyhow::Result<MasterKey> {
    use std::io::Write;
    let key = MasterKey::generate()?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("cannot create {} (already exists?)", path.display()))?;
    f.write_all(key.encode().as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> Config {
        Config {
            database_url: String::new(),
            redis_url: String::new(),
            public_url: url.parse().unwrap(),
            listen: "127.0.0.1:0".parse().unwrap(),
            master_key_file: PathBuf::new(),
            trusted_proxies: vec![],
            node_listen: "127.0.0.1:7101".parse().unwrap(),
            node_url: "https://localhost:7101".parse().unwrap(),
        }
    }

    #[test]
    fn public_url_rules() {
        assert!(cfg("https://cloud.example.com").validate().is_ok());
        assert!(cfg("http://localhost:5173").validate().is_ok());
        assert!(cfg("http://cloud.example.com").validate().is_err());
        assert!(cfg("ftp://x").validate().is_err());
        assert_eq!(
            cfg("https://cloud.example.com/").public_origin(),
            "https://cloud.example.com"
        );
        assert_eq!(
            cfg("http://localhost:5173").public_origin(),
            "http://localhost:5173"
        );
    }

    #[test]
    fn master_key_file_permissions() {
        let dir = std::env::temp_dir().join(format!("modula-key-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("master.key");
        let k = write_new_master_key(&path).unwrap();
        assert_eq!(load_master_key(&path).unwrap().id(), k.id());
        assert!(write_new_master_key(&path).is_err(), "must not overwrite");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load_master_key(&path).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
