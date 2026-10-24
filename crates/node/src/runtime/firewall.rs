//! Host firewall rules for containers: no container reaches the cloud's
//! instance metadata service (which hands out credentials and user data on
//! AWS, GCP, Hetzner and others).
//!
//! Docker sends container traffic through the FORWARD chain, which jumps to
//! `DOCKER-USER` first; rules there survive Docker's own rewrites. The
//! node's own metadata lookups (`location.rs`) come from the host itself
//! (OUTPUT, never FORWARD), so they keep working.
//!
//! Idempotent (`-C` before `-I`), argv only, and never fatal: without
//! iptables or the chain (not Linux, Docker's nftables backend) it warns.

use crate::access::run;

const CHAIN: &str = "DOCKER-USER";
/// (binary, destination): the IPv4 metadata address and AWS's IPv6 one.
const BLOCKED: &[(&str, &str)] = &[
    ("iptables", "169.254.169.254/32"),
    ("ip6tables", "fd00:ec2::254/128"),
];

/// The rule for `dest`, after the verb and chain: `-d <dest> -j DROP`.
pub fn rule(dest: &str) -> [&str; 4] {
    ["-d", dest, "-j", "DROP"]
}

/// Make sure the metadata service is dropped for containers.
pub async fn block_metadata() {
    if !cfg!(target_os = "linux") {
        return;
    }
    for (bin, dest) in BLOCKED {
        if let Err(e) = ensure_drop(bin, dest).await {
            tracing::warn!(
                firewall = bin,
                error = format!("{e:#}"),
                "cannot block the metadata service for containers"
            );
        }
    }
}

/// [`block_metadata`] in the background (after a network is created).
pub fn spawn_block_metadata() {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(block_metadata());
    }
}

async fn ensure_drop(bin: &str, dest: &str) -> anyhow::Result<()> {
    // `-C` also fails when the chain is missing: check that first.
    run(bin, &["-w", "-S", CHAIN])
        .await
        .map_err(|e| e.context(format!("no {CHAIN} chain (is Docker running?)")))?;
    let mut check = vec!["-w", "-C", CHAIN];
    check.extend(rule(dest));
    if run(bin, &check).await.is_ok() {
        return Ok(());
    }
    let mut insert = vec!["-w", "-I", CHAIN];
    insert.extend(rule(dest));
    run(bin, &insert).await?;
    tracing::info!(firewall = bin, %dest, "blocked the metadata service for containers");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_both_metadata_addresses() {
        assert_eq!(
            rule("169.254.169.254/32"),
            ["-d", "169.254.169.254/32", "-j", "DROP"]
        );
        let dests: Vec<&str> = BLOCKED.iter().map(|(_, d)| *d).collect();
        assert_eq!(dests, ["169.254.169.254/32", "fd00:ec2::254/128"]);
    }

    #[tokio::test]
    async fn never_fails_without_iptables() {
        block_metadata().await;
    }
}
