//! The user data a provisioned server boots with: a shell script cloud-init
//! runs once as root. It downloads this instance's installer and joins in
//! worker mode (Docker, Caddy, modula-node; no Postgres/Redis/dashboard).
//!
//! The only secret in it is the single-use, 30-minute join token. Every
//! value is single-quoted for the shell even though each is validated to a
//! safe character set first.

use crate::config::Config;

pub struct Bootstrap<'a> {
    /// `https://<dashboard>/api/install.sh`
    pub install_url: String,
    /// The node endpoint (`--server`).
    pub server: String,
    pub token: &'a str,
    /// `sha256:<hex>` of the node CA.
    pub ca: &'a str,
    pub name: &'a str,
}

impl<'a> Bootstrap<'a> {
    pub fn new(cfg: &Config, token: &'a str, ca: &'a str, name: &'a str) -> Self {
        Self {
            install_url: format!("{}/api/install.sh", cfg.public_origin()),
            server: cfg.node_url.as_str().trim_end_matches('/').to_owned(),
            token,
            ca,
            name,
        }
    }

    pub fn render(&self) -> String {
        let q = shell_quote;
        format!(
            r#"#!/bin/bash
# Modula Deploy worker bootstrap, run once by cloud-init as root.
# Installs Docker, Caddy and modula-node, then joins the control plane over
# mutual TLS. The join token is single-use and expires 30 minutes after the
# server was requested; --ca pins the certificate authority of the control plane.
set -euo pipefail
installer=$(mktemp)
curl -fsSL --retry 10 --retry-delay 3 --retry-connrefused --retry-all-errors -o "$installer" {url}
bash "$installer" join --server {server} --token {token} --ca {ca} --name {name}
rm -f "$installer"
# The token is spent; drop the copies cloud-init keeps of this script anyway.
rm -f /var/lib/cloud/instance/user-data.txt /var/lib/cloud/instance/user-data.txt.i /var/lib/cloud/instance/scripts/part-* || true
"#,
            url = q(&self.install_url),
            server = q(&self.server),
            token = q(self.token),
            ca = q(self.ca),
            name = q(self.name),
        )
    }
}

/// POSIX single quotes: nothing inside is special except `'` itself.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bootstrap<'static> {
        Bootstrap {
            install_url: "https://deploy.example.com/api/install.sh".into(),
            server: "https://deploy.example.com:7101".into(),
            token: "mcj_AbC-123_xyz",
            ca: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            name: "worker-1",
        }
    }

    #[test]
    fn renders_a_quoted_worker_join() {
        let s = sample().render();
        assert!(s.starts_with("#!/bin/bash\n"));
        assert!(s.contains(
            "bash \"$installer\" join --server 'https://deploy.example.com:7101' --token 'mcj_AbC-123_xyz' --ca 'sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' --name 'worker-1'"
        ));
        assert!(s.contains("-o \"$installer\" 'https://deploy.example.com/api/install.sh'"));
        // Worker mode only: never the main-server install.
        assert!(!s.contains("--domain"));
        assert!(!s.contains("sudo"));
    }

    #[test]
    fn carries_no_secret_but_the_join_token() {
        let s = sample().render();
        // Every quoted value is one of the inputs; nothing else is interpolated.
        let quoted: Vec<&str> = s.split('\'').skip(1).step_by(2).collect();
        let b = sample();
        let allowed = [
            b.install_url.as_str(),
            b.server.as_str(),
            b.token,
            b.ca,
            b.name,
        ];
        assert_eq!(quoted.len(), allowed.len());
        for q in quoted {
            assert!(allowed.contains(&q), "unexpected value {q}");
        }
        for word in ["password", "secret", "mdt_", "mcs_", "BEGIN"] {
            assert!(!s.contains(word), "{word} in user data");
        }
    }

    #[test]
    fn quoting_is_shell_safe() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("a`b`;c"), "'a`b`;c'");
        let mut b = sample();
        b.name = "x'; reboot; '";
        let s = b.render();
        assert!(s.contains(r"--name 'x'\''; reboot; '\'''"));
    }
}
