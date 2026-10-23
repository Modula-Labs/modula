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
    pub domain: &'a str,
    pub email: &'a str,
    pub name: &'a str,
    pub setup_token: &'a str,
    /// Names the installer's progress file (see `install_progress`).
    pub progress_token: &'a str,
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
installer=$(mktemp)
curl -fsSL --retry 10 --retry-delay 3 --retry-connrefused --retry-all-errors -o "$installer" {url}
bash "$installer" --domain {domain} --email {email} --name {name} --setup-token {token} --progress-token {progress}
rm -f "$installer"
# Drop the copies cloud-init keeps of this script (they hold the setup token).
rm -f /var/lib/cloud/instance/user-data.txt /var/lib/cloud/instance/user-data.txt.i /var/lib/cloud/instance/scripts/part-* || true
"#,
            url = q(self.install_url),
            domain = q(self.domain),
            email = q(self.email),
            name = q(self.name),
            token = q(self.setup_token),
            progress = q(self.progress_token),
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

    const TOKEN: &str = "mcsetup_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde";

    fn sample() -> MainBootstrap<'static> {
        MainBootstrap {
            install_url: INSTALL_URL,
            domain: "deploy.example.com",
            email: "you@example.com",
            name: "modula-main",
            setup_token: TOKEN,
            progress_token: "PrOgReSs0123456789abcdefghijklmnopqrstuvwxy",
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
            b.domain,
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
