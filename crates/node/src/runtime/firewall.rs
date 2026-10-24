//! Host firewall rules for workloads: no container reaches the cloud's
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

/// (binary, destination) a function's user may not open connections to:
/// anything but the public internet (see `crate::functions`; workerd's
/// network config already refuses them, this is the host's backstop).
const FUNCTION_BLOCKED: &[(&str, &str)] = &[
    ("iptables", "10.0.0.0/8"),
    ("iptables", "100.64.0.0/10"),
    ("iptables", "127.0.0.0/8"),
    ("iptables", "169.254.0.0/16"),
    ("iptables", "172.16.0.0/12"),
    ("iptables", "192.168.0.0/16"),
    ("ip6tables", "::1/128"),
    ("ip6tables", "fc00::/7"),
    ("ip6tables", "fe80::/10"),
];

/// In OUTPUT: new connections from `uid` to `dest` are refused. Replies on
/// connections made to it (Caddy's requests, over loopback) still pass.
pub fn uid_rule<'a>(uid: &'a str, dest: &'a str) -> [&'a str; 12] {
    [
        "-m",
        "owner",
        "--uid-owner",
        uid,
        "-m",
        "conntrack",
        "--ctstate",
        "NEW",
        "-d",
        dest,
        "-j",
        "REJECT",
    ]
}

/// Keeps the functions' user (`uid`) off this server, its networks and the
/// metadata service. Idempotent; warns without iptables.
pub async fn isolate_uid(uid: u32) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let uid = uid.to_string();
    for (bin, dest) in FUNCTION_BLOCKED {
        let mut check = vec!["-w", "-C", "OUTPUT"];
        check.extend(uid_rule(&uid, dest));
        if run(bin, &check).await.is_ok() {
            continue;
        }
        let mut insert = vec!["-w", "-I", "OUTPUT"];
        insert.extend(uid_rule(&uid, dest));
        if let Err(e) = run(bin, &insert).await {
            tracing::warn!(
                firewall = bin,
                %dest,
                error = format!("{e:#}"),
                "cannot keep functions off a private range"
            );
        }
    }
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

    #[test]
    fn functions_reach_only_the_public_internet() {
        assert_eq!(
            uid_rule("998", "169.254.0.0/16").join(" "),
            "-m owner --uid-owner 998 -m conntrack --ctstate NEW -d 169.254.0.0/16 -j REJECT"
        );
        for d in ["127.0.0.0/8", "10.0.0.0/8", "169.254.0.0/16", "fc00::/7"] {
            assert!(FUNCTION_BLOCKED.iter().any(|(_, b)| *b == d), "{d}");
        }
    }

    #[tokio::test]
    async fn never_fails_without_iptables() {
        block_metadata().await;
        isolate_uid(998).await;
    }
}
