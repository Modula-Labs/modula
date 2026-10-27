//! Finding out where this server runs, for the dashboard's location label.
//!
//! Asks the cloud metadata services that answer on well-known link-local
//! addresses (each with a short timeout, all at once), and Cloudflare's trace
//! endpoint for the nearest edge as a fallback. Nothing here is trusted for
//! security; it only picks a label, which admins can override.

use std::time::Duration;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Hints {
    pub provider: String,
    pub region: String,
    pub colo: String,
    /// The public IPv4 this server reaches the internet from.
    pub public_ip: String,
}

const METADATA_TIMEOUT: Duration = Duration::from_millis(700);

pub async fn detect() -> Hints {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Ok(http) = reqwest::Client::builder()
        .timeout(METADATA_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        return Hints::default();
    };
    let (hetzner, aws, gcp, digitalocean, azure, edge) = tokio::join!(
        text(
            &http,
            "http://169.254.169.254/hetzner/v1/metadata/availability-zone",
            &[]
        ),
        aws_region(&http),
        text(
            &http,
            "http://metadata.google.internal/computeMetadata/v1/instance/zone",
            &[("Metadata-Flavor", "Google")]
        ),
        text(&http, "http://169.254.169.254/metadata/v1/region", &[]),
        text(
            &http,
            "http://169.254.169.254/metadata/instance/compute/location?api-version=2021-02-01&format=text",
            &[("Metadata", "true")]
        ),
        edge_trace(),
    );
    let (provider, region) = if let Some(r) = hetzner {
        ("hetzner", r)
    } else if let Some(r) = aws {
        ("aws", r)
    } else if let Some(zone) = gcp {
        // projects/123/zones/europe-west3-a → europe-west3
        let zone = zone.rsplit('/').next().unwrap_or_default();
        (
            "gcp",
            zone.rsplit_once('-').map_or(zone, |(r, _)| r).to_owned(),
        )
    } else if let Some(r) = digitalocean {
        ("digitalocean", r)
    } else if let Some(r) = azure {
        ("azure", r)
    } else {
        ("", String::new())
    };
    Hints {
        provider: provider.into(),
        region,
        colo: edge.colo,
        public_ip: edge.ip,
    }
}

/// A short plain-text answer, or `None` (wrong cloud, timeout, junk).
async fn text(http: &reqwest::Client, url: &str, headers: &[(&str, &str)]) -> Option<String> {
    let mut req = http.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    clean(&resp.text().await.ok()?)
}

/// Region names are short and plain; anything else is someone else's page.
fn clean(s: &str) -> Option<String> {
    let s = s.trim();
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_/.".contains(&b));
    ok.then(|| s.to_owned())
}

async fn aws_region(http: &reqwest::Client) -> Option<String> {
    // IMDSv2: a session token first.
    let token = http
        .put("http://169.254.169.254/latest/api/token")
        .header("X-aws-ec2-metadata-token-ttl-seconds", "60")
        .send()
        .await
        .ok()
        .filter(|r| r.status().is_success())?
        .text()
        .await
        .ok()?;
    text(
        http,
        "http://169.254.169.254/latest/meta-data/placement/region",
        &[("X-aws-ec2-metadata-token", token.trim())],
    )
    .await
}

#[derive(Default)]
struct Edge {
    colo: String,
    ip: String,
}

/// The nearest Cloudflare edge and our public IPv4, from Cloudflare's trace.
async fn edge_trace() -> Edge {
    let Some(body) = trace_body().await else {
        return Edge::default();
    };
    let field = |k: &str| {
        body.lines()
            .find_map(|l| l.strip_prefix(k))
            .map(str::trim)
            .unwrap_or("")
    };
    let colo = clean(field("colo="))
        .filter(|c| c.len() == 3)
        .unwrap_or_default();
    let ip = field("ip=")
        .parse::<std::net::Ipv4Addr>()
        .map(|ip| ip.to_string())
        .unwrap_or_default();
    Edge { colo, ip }
}

async fn trace_body() -> Option<String> {
    // Cloudflare turns away requests without a User-Agent.
    let http = reqwest::Client::builder()
        .user_agent(concat!("modula-node/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    // By IPv4 address: over IPv6 the trace would report the v6 address and
    // the server's public IPv4 (what DNS records point at) stay unknown.
    let body = http
        .get("https://1.1.1.1/cdn-cgi/trace")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    Some(body)
}

#[cfg(test)]
mod tests {
    #[test]
    fn junk_is_not_a_region() {
        assert_eq!(super::clean(" fsn1-dc14\n").as_deref(), Some("fsn1-dc14"));
        assert_eq!(super::clean("<html>nope</html>"), None);
        assert_eq!(super::clean(""), None);
    }
}
