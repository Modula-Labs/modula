//! The user data a new main server boots with: a shell script cloud-init
//! runs once as root. It downloads the installer and installs Modula in main
//! mode without questions (`--domain --email --name --setup-token`).
//!
//! The only secret in it is the one-time setup token, which is useless once
//! `modula server create` has finished setup with it (minutes later). The
//! Hetzner and Cloudflare tokens never go near it: the renderer doesn't get
//! them. Every value is single-quoted for the shell even though each is
//! validated to a safe character set first.

pub const INSTALL_URL: &str = "https://usemodula.org/install.sh";

pub struct MainBootstrap<'a> {
    /// Where install.sh comes from (normally [`INSTALL_URL`]).
    pub install_url: &'a str,
    /// None: no domain yet; the installer serves the dashboard at
    /// `https://<public IP>` and a domain is added later in the dashboard.
    pub domain: Option<&'a str>,
    pub email: &'a str,
    pub name: &'a str,
    pub setup_token: &'a str,
    /// Names the installer's progress file (see `install_progress`).
    pub progress_token: &'a str,
    /// An SSH key was installed for root: turn password and keyboard-
    /// interactive logins off (nothing can be locked out).
    pub key_only_ssh: bool,
}

impl MainBootstrap<'_> {
    pub fn render(&self) -> String {
        let q = shell_quote;
        format!(
            r#"#!/bin/bash
# Modula Deploy main server bootstrap, run once by cloud-init as root
# (created by `modula server create`). Installs the dashboard, Postgres,
# Redis, Docker, Caddy and modula-node without questions. The setup token is
# single use: the modula CLI finishes setup with it as soon as the dashboard
# answers. Progress: /var/log/cloud-init-output.log
set -euo pipefail
{ssh_setup}installer=$(mktemp)
curl -fsSL --retry 10 --retry-delay 3 --retry-connrefused --retry-all-errors -o "$installer" {url}
bash "$installer" {domain_args}--email {email} --name {name} --setup-token {token} --progress-token {progress}
rm -f "$installer"
# Drop the copies cloud-init keeps of this script (they hold the setup token).
rm -f /var/lib/cloud/instance/user-data.txt /var/lib/cloud/instance/user-data.txt.i /var/lib/cloud/instance/scripts/part-* || true
"#,
            url = q(self.install_url),
            ssh_setup = if self.key_only_ssh { KEY_ONLY_SSH } else { "" },
            domain_args = match self.domain {
                Some(d) => format!("--domain {} ", q(d)),
                None => String::new(),
            },
            email = q(self.email),
            name = q(self.name),
            token = q(self.setup_token),
            progress = q(self.progress_token),
        )
    }
}

/// Key-only SSH, first in sshd_config.d so it wins over cloud-init's
/// `50-cloud-init.conf` (sshd keeps the first value it reads).
const KEY_ONLY_SSH: &str = r#"printf "PasswordAuthentication no\nKbdInteractiveAuthentication no\nX11Forwarding no\n" > /etc/ssh/sshd_config.d/01-modula-key-only.conf
sshd -t && { systemctl reload ssh 2>/dev/null || systemctl reload sshd 2>/dev/null || true; }
"#;

/// POSIX single quotes: nothing inside is special except `'` itself.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "mcsetup_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde";

    fn sample() -> MainBootstrap<'static> {
        MainBootstrap {
            install_url: INSTALL_URL,
            domain: Some("deploy.example.com"),
            email: "you@example.com",
            name: "modula-main",
            setup_token: TOKEN,
            progress_token: "PrOgReSs0123456789abcdefghijklmnopqrstuvwxy",
            key_only_ssh: true,
        }
    }

    #[test]
    fn renders_a_quoted_main_install() {
        let s = sample().render();
        assert!(s.starts_with("#!/bin/bash\n"));
        assert!(s.contains(&format!(
            "bash \"$installer\" --domain 'deploy.example.com' --email 'you@example.com' --name 'modula-main' --setup-token '{TOKEN}' --progress-token 'PrOgReSs0123456789abcdefghijklmnopqrstuvwxy'"
        )));
        assert!(s.contains("-o \"$installer\" 'https://usemodula.org/install.sh'"));
        // Main mode: never a worker join.
        assert!(!s.contains(" join "));
    }

    #[test]
    fn carries_no_secret_but_the_setup_token() {
        let s = sample().render();
        let quoted: Vec<&str> = s.split('\'').skip(1).step_by(2).collect();
        let b = sample();
        let allowed = [
            b.install_url,
            b.domain.unwrap(),
            b.email,
            b.name,
            b.setup_token,
            b.progress_token,
        ];
        assert_eq!(quoted.len(), allowed.len());
        for q in quoted {
            assert!(allowed.contains(&q), "unexpected value {q}");
        }
        for word in ["password", "mdt_", "HCLOUD", "hcloud", "Bearer", "BEGIN"] {
            assert!(!s.contains(word), "{word} in user data");
        }
    }

    #[test]
    fn key_only_ssh_only_with_a_key() {
        let s = sample().render();
        assert!(s.contains("PasswordAuthentication no"));
        assert!(s.contains("sshd -t &&"));
        let mut b = sample();
        b.key_only_ssh = false;
        assert!(!b.render().contains("sshd_config.d"));
    }

    #[test]
    fn no_domain_installs_on_the_ip() {
        let mut b = sample();
        b.domain = None;
        let s = b.render();
        // The installer finds the public IP itself; nothing third-party.
        assert!(s.contains(r#"bash "$installer" --email 'you@example.com'"#));
        assert!(!s.contains("--domain") && !s.contains("--apps-domain"));
        assert_eq!(
            crate::provision::flow::instant_domain("203.0.113.7".parse().unwrap()),
            "203.0.113.7"
        );
    }

    #[test]
    fn quoting_is_shell_safe() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_quote("a`b`;c"), "'a`b`;c'");
        let mut b = sample();
        b.email = "x'; reboot; '@example.com";
        let s = b.render();
        assert!(s.contains(r"--email 'x'\''; reboot; '\''@example.com'"));
    }
}
