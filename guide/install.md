# Install

The main server runs the dashboard and API (`modula-server`), its Postgres and
Redis, and is also the first server your apps run on (`modula-node`, Docker,
Caddy). More servers can [join](servers.md) later.

## Requirements

- A fresh **Ubuntu 22.04+** or **Debian 12+** server with systemd, `amd64` or
  `arm64`, and root access.
- A hostname for the dashboard, e.g. `deploy.example.com`, with an `A` record
  pointing at the server.
- Optional, for generated app domains: a parent domain such as
  `apps.example.com` with a wildcard record `*.apps.example.com` pointing at
  the server.
- An email address for Let's Encrypt.

Nothing is compiled during install. The installer downloads prebuilt static
binaries, checks them against SHA-256 sums signed with the release key, and
installs Postgres, Redis and (if missing) Docker from the distribution's
packages.

## Ports

| Port | Who needs it |
|---|---|
| 80, 443 (TCP) | Everyone: dashboard, your apps' domains, certificate issuance. |
| 7101 (TCP) | Servers joining this instance (mutual TLS). Only the main server. |
| 20000–29999 (TCP) | Only if you enable [TCP proxies](databases.md#public-access-tcp-proxy) for databases. |

The API itself listens on `127.0.0.1:7100` behind Caddy.

## Install the main server

```bash
curl -fsSL https://github.com/modula-deploy/modula-deploy/releases/latest/download/install.sh \
  | sudo bash -s -- --domain deploy.example.com --email you@example.com --apps-domain apps.example.com
```

| Option | Description |
|---|---|
| `--domain HOST` | Dashboard hostname (required). DNS must point here. |
| `--email ADDRESS` | Contact for Let's Encrypt (required). |
| `--apps-domain HOST` | Parent for generated app domains. Needs `*.HOST` pointing here. Without it, only custom domains work. |
| `--version X.Y.Z` | Install this version instead of the latest. |

Every step is idempotent: if a run fails, fix the cause and run the same
command again. When it finishes it prints:

- the dashboard URL,
- a single-use **setup token**: open the dashboard and create the first admin
  account (and workspace) with it,
- the path of the **recovery kit**.

Then enable [two-factor authentication](security.md#two-factor-authentication)
on your account.

## Recovery kit

`/root/modula-recovery-kit.txt` holds the **master key** that encrypts every
secret on the instance (variables, deployment snapshots, the internal CA,
backup keys). Without it, a database backup of the instance can't be
decrypted.

1. Copy the file somewhere safe and offline (a password manager).
2. Delete it from the server: `shred -u /root/modula-recovery-kit.txt`.

To restore onto a fresh install, write the key to
`/var/lib/modula/master.key` (mode 0600, owner `modula`) before starting
`modula-server`.

## Where things live

| Path | What |
|---|---|
| `/etc/modula/server.env` | Server configuration (`MODULA_*` variables) |
| `/var/lib/modula/master.key` | Master key |
| `/opt/modula/current` | The active release (the last 3 are kept) |
| `/var/lib/modula-node`, `/var/lib/modula-caddy` | Node identity and edge state |

Services: `modula-server`, `modula-node`, `modula-caddy` (systemd).

### Server configuration

Set in `/etc/modula/server.env`; restart `modula-server` after a change.

| Variable | Description |
|---|---|
| `MODULA_PUBLIC_URL` | The dashboard's URL. Every link and the CSRF check use it. |
| `MODULA_APPS_DOMAIN` | Parent for generated domains; empty disables them. |
| `MODULA_TCP_PROXY_HOST` | Host clients use for TCP proxies (default: each server's public IP). |
| `MODULA_UPLOADS_DIR` | Where `modula up` archives are kept (default: `uploads/` next to the master key; share it between server replicas). |
| `MODULA_UPLOAD_MAX_MB` | Largest accepted `up` archive, compressed (default 200). |
| `MODULA_INTERNAL_URL` | URL servers use to download uploaded sources from this server (default: `MODULA_PUBLIC_URL`). |
| `MODULA_EDGE_TLS` | How the edge gets certificates: `acme` (default), `internal`, `off`. |
| `MODULA_ACME_EMAIL` | ACME contact address. |
| `MODULA_NODE_LISTEN` | Listener for servers (default `0.0.0.0:7101`). |
| `MODULA_NODE_URL` | Address joining servers dial (default: the dashboard host, port 7101). |
| `MODULA_LISTEN` | API listener (`127.0.0.1:7100`). |
| `MODULA_DATABASE_URL`, `MODULA_REDIS_URL` | Control-plane Postgres and Redis. |
| `MODULA_MASTER_KEY_FILE` | Path of the master key. |
| `MODULA_TRUSTED_PROXIES` | Peers whose `X-Forwarded-For` is trusted (the local Caddy). |

## Upgrade

Upgrade the main server first, from the release:

```bash
curl -fsSL https://github.com/modula-deploy/modula-deploy/releases/latest/download/install.sh \
  | sudo bash -s -- upgrade [--version X.Y.Z]
```

Then each other server, from your instance (its installer installs the
instance's own version):

```bash
curl -fsSL https://deploy.example.com/api/install.sh | sudo bash -s -- upgrade
```

The release is switched atomically, and the main server rolls back
automatically if the new version doesn't become healthy. Running apps keep
running during the upgrade.
