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
    let (hetzner, aws, gcp, digitalocean, azure, colo) = tokio::join!(
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
        edge_colo(),
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
        colo: colo.unwrap_or_default(),
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

async fn edge_colo() -> Option<String> {
    // Cloudflare turns away requests without a User-Agent.
    let http = reqwest::Client::builder()
        .user_agent(concat!("modula-node/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let body = http
        .get("https://www.cloudflare.com/cdn-cgi/trace")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    body.lines()
        .find_map(|l| l.strip_prefix("colo="))
        .and_then(clean)
        .filter(|c| c.len() == 3)
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
