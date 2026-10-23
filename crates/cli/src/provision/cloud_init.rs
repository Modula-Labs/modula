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
    /// None: an instant `<ip>.sslip.io` address, derived on the server from
    /// Hetzner's metadata (apps get `<name>.<ip>.sslip.io`).
    pub domain: Option<&'a str>,
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
{domain_setup}installer=$(mktemp)
curl -fsSL --retry 10 --retry-delay 3 --retry-connrefused --retry-all-errors -o "$installer" {url}
bash "$installer" {domain_args} --email {email} --name {name} --setup-token {token} --progress-token {progress}
rm -f "$installer"
# Drop the copies cloud-init keeps of this script (they hold the setup token).
rm -f /var/lib/cloud/instance/user-data.txt /var/lib/cloud/instance/user-data.txt.i /var/lib/cloud/instance/scripts/part-* || true
"#,
            url = q(self.install_url),
            domain_setup = match self.domain {
                Some(_) => String::new(),
                None => INSTANT_DOMAIN.to_owned(),
            },
            domain_args = match self.domain {
                Some(d) => format!("--domain {}", q(d)),
                None => r#"--domain "$domain" --apps-domain "$domain""#.to_owned(),
            },
            email = q(self.email),
            name = q(self.name),
            token = q(self.setup_token),
            progress = q(self.progress_token),
        )
    }
}

/// The public IPv4 from Hetzner's metadata service, as `1-2-3-4.sslip.io`
/// (`flow::instant_domain` derives the same name).
const INSTANT_DOMAIN: &str = r#"ip=$(curl -fsS --retry 10 --retry-delay 2 --retry-all-errors http://169.254.169.254/hetzner/v1/metadata/public-ipv4)
[[ $ip =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "no public IPv4 in the metadata: $ip" >&2; exit 1; }
domain="${ip//./-}.sslip.io"
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
    fn instant_address_comes_from_the_metadata() {
        let mut b = sample();
        b.domain = None;
        let s = b.render();
        assert!(s.contains("http://169.254.169.254/hetzner/v1/metadata/public-ipv4"));
        assert!(s.contains(r#"--domain "$domain" --apps-domain "$domain" --email"#));
        // Same name as the CLI's `flow::instant_domain`: dots become dashes.
        assert!(s.contains(r#"domain="${ip//./-}.sslip.io""#));
        assert_eq!(
            crate::provision::flow::instant_domain("203.0.113.7".parse().unwrap()),
            "203-0-113-7.sslip.io"
        );
        assert!(s.contains(r"^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$"));
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
