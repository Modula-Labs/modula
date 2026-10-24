# Security policy

## Reporting a vulnerability

Please report vulnerabilities **privately**. Do not open a public issue, discussion
or pull request for a security problem.

1. **Preferred:** GitHub private vulnerability reporting. On the repository, open
   **Security → Report a vulnerability**
   (<https://github.com/Modula-Labs/modula/security/advisories/new>).
2. **Fallback:** email **security@usemodula.org**.
   <!-- OWNER: confirm this address exists and is monitored before launch. -->

Please include the affected version (`modula-server --version`, `modula-node --version`
or `modula --version`), the component, steps to reproduce, and the impact you expect.
A proof of concept helps but is not required.

### What to expect

| Step | Target |
| --- | --- |
| Acknowledgement of your report | within 3 business days |
| Initial assessment (confirmed / not a vulnerability / need more info) | within 10 business days |
| Fix released for confirmed critical or high-severity issues | as fast as we can; we keep you updated at least weekly |

We coordinate disclosure with you, publish a GitHub Security Advisory with the fix,
and credit you unless you ask us not to. Please give us a reasonable window to ship
a fix before disclosing publicly.

## Supported versions

Modula Deploy is pre-1.0. Only the **latest 0.x release** receives security fixes;
upgrade to it before reporting (the control plane can update itself and its servers
from **Settings → Updates**).

| Version | Supported |
| --- | --- |
| latest 0.x release | yes |
| older releases | no |

## Project status

Modula Deploy is **alpha** software. Today it is designed for **single-owner
self-hosting**: one person (or one trusted team sharing the owner account) runs the
control plane and its servers. Multi-user workspaces and invites are not available
yet, so the authorization model between multiple, mutually untrusted users of one
instance is not yet a supported security boundary.

## Scope

In scope:

- **Control plane** (`modula-server`): the HTTP API, the web UI it embeds, sessions,
  2FA and sudo mode, API tokens, the node gRPC endpoint and its PKI, secrets at rest,
  webhooks, backups, and integrations (GitHub, Cloudflare, Hetzner).
- **Server daemon** (`modula-node`): joining over mTLS, running workloads, builds
  (BuildKit, Railpack), the Caddy edge it configures, logs and metrics.
- **Installer** (`deploy/install.sh`, `scripts/install-cli.sh`) and release
  verification.
- **CLI** (`modula`): token storage, `modula up`, `modula connect`, config as code.

Out of scope:

- Vulnerabilities in the applications you deploy, or in upstream images.
- Vulnerabilities in third-party components (Caddy, BuildKit, Railpack, Docker,
  Postgres, Redis) unless Modula Deploy's configuration or use of them makes the
  problem worse. Please report those upstream as well.
- Attacks that need root on the control-plane host, or the owner's credentials and
  second factor.
- Denial of service through sheer volume, and findings from automated scanners
  without a demonstrated impact.

## Known limitations and accepted risks

These are known and documented; reports that only restate them are not treated as
new vulnerabilities, but ideas for closing them are welcome.

- **A compromised server can misreport its public IP.** Each server (node) reports
  its own public address. A server that is fully compromised can report a wrong
  address and so hold up or pause rollouts that depend on it (for example DNS
  records or health checks). The node endpoint still only lets it report on
  deployments placed on it, and revoking the server cuts it off.
- **Containers run as the image's user.** Modula Deploy does not enable Docker
  user-namespace remapping by default; a container whose image runs as root is root
  inside the container. Use images that run as an unprivileged user for untrusted
  workloads.
- **Alpha, single owner.** See "Project status" above.

## Release signing

Release artifacts are published at `downloads.usemodula.org`. Every release has a
`SHA256SUMS` file listing each artifact (and `VERSION`), and `SHA256SUMS.sig`, an
Ed25519 signature over that file. The public key is committed at
[`deploy/release.pub.pem`](deploy/release.pub.pem) and embedded in the installer and
the binaries' update checks; the private key never enters CI, a container or this
repository. The installer and the self-updater refuse artifacts whose signature or
checksum does not verify. To verify by hand:

```bash
openssl pkeyutl -verify -pubin -inkey deploy/release.pub.pem -rawin \
  -in SHA256SUMS -sigfile SHA256SUMS.sig
sha256sum --check --ignore-missing SHA256SUMS
```

## Security design

The security model (authentication, sudo mode, API tokens, secret handling, node
PKI, protected environments) is described in [guide/security.md](guide/security.md).
