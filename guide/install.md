# Install

The main server runs the dashboard and API (`modula-server`), its Postgres and
Redis, and is also the first server your apps run on (`modula-node`, Docker,
Caddy). More servers can [join](servers.md) later.

## Requirements

- A fresh **Ubuntu 22.04+** or **Debian 12+** server with systemd, `amd64` or
  `arm64`, and root or `sudo` access.
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
| 22 (TCP), or your SSH port | You. The installer's firewall keeps whatever port SSH uses open. |
| 80, 443 (TCP) | Everyone: dashboard, your apps' domains, certificate issuance. |
| 443 (UDP) | HTTP/3 for the dashboard and your apps. |
| 7101 (TCP) | Servers joining this instance (mutual TLS). Only the main server. |
| 20000–29999 (TCP) | Only if you enable [TCP proxies](databases.md#public-access-tcp-proxy) for databases. |

Other servers need the same, minus 7101: they only dial out to the main
server. The API itself listens on `127.0.0.1:7100` behind Caddy; Postgres and
Redis listen only on localhost (Postgres on its unix socket and loopback,
Redis on loopback with a password). Containers publish no ports: Caddy and
the TCP proxies listen on the host and forward to them.

## Install the main server

```bash
curl -fsSL https://usemodula.org/install.sh | bash
```

It asks for:

- the **dashboard domain**, e.g. `deploy.example.com` (it shows this server's
  public IP for the `A` record and warns if the domain doesn't point here yet),
- an **email** for TLS certificates (Let's Encrypt),
- a **server name** for the Servers page (default: the short hostname),

then confirms before changing anything. Not running as root? It asks `sudo`
to run itself again as root (fetching the script once more from
`https://usemodula.org/install.sh`, since a piped script has no file).

For automation (cloud-init, CI) pass the answers as flags; with `--domain` and
`--email` it asks nothing:

```bash
curl -fsSL https://usemodula.org/install.sh \
  | sudo bash -s -- --domain deploy.example.com --email you@example.com --apps-domain apps.example.com
```

| Option | Description |
|---|---|
| `--domain HOST` | Dashboard hostname (asked when missing). DNS must point here. |
| `--email ADDRESS` | Contact for Let's Encrypt (asked when missing). |
| `--name NAME` | This server's name on the Servers page: letters, digits, `.` `_` `-` (default: short hostname). |
| `--apps-domain HOST` | Parent for generated app domains. Needs `*.HOST` pointing here. Without it, only custom domains work. |
| `--version X.Y.Z` | Install this version instead of the latest. |
| `--no-harden` | Skip the [host setup](#host-setup) entirely. |
| `--no-firewall` | Skip only the firewall. |
| `--auto-reboot` | Let security updates reboot the server at 04:00 when they need to. |

Every step is idempotent: if a run fails, fix the cause and run the same
command again. When it finishes it prints:

- the dashboard URL,
- a single-use **setup token**: open the dashboard and create the first admin
  account (and workspace) with it,
- the path of the **recovery kit**.

Then enable [two-factor authentication](security.md#two-factor-authentication)
on your account.

## Host setup

Installs (the main server and every server that joins, but never `upgrade`)
also set up the machine. The interactive install asks once, "Set up the
firewall and basic hardening?"; joining servers and installs driven by flags
do it unless you pass `--no-harden`. Every part is idempotent and leaves
existing settings alone where noted.

| What | Details |
|---|---|
| Firewall (ufw) | Denies incoming traffic except the ports above. Your SSH port (from `sshd -T`, systemd's `ssh.socket`, or the current SSH session; 22 otherwise) is allowed before the firewall is turned on. An already active ufw keeps its rules and defaults; Modula's rules are added (comment `modula`). Skipped, with a note, when firewalld or an nftables ruleset that drops traffic is in charge. |
| Security updates | `unattended-upgrades`, daily, security pockets only (`/etc/apt/apt.conf.d/52modula-unattended-upgrades`), no reboots unless `--auto-reboot`. A server that already runs unattended-upgrades keeps its own settings. |
| Logs | journald capped at 500 MB (`/etc/systemd/journald.conf.d/90-modula.conf`). Docker's default log driver rotates at 3 × 10 MB (`/etc/docker/daemon.json`: written when absent, merged with `jq` otherwise, never overwritten; Docker restarts for it only when the installer just installed it). Modula's own containers always rotate their logs. |
| Swap | With less than 2 GiB of memory and no swap, a 2 GiB `/swapfile` (in `/etc/fstab`) and `vm.swappiness = 10`. |
| Clock | Makes sure systemd-timesyncd (or chrony/ntp, if present) keeps the clock in sync; certificates and mutual TLS depend on it. |
| Kernel | `/etc/sysctl.d/90-modula.conf`: `net.core.somaxconn = 4096`, `fs.inotify.max_user_watches = 524288`, `fs.inotify.max_user_instances = 1024`, `net.ipv4.ip_forward = 1` (containers reach the internet through the host). |
| SSH | Never changed. If it accepts passwords, the installer prints how to switch to key-only logins. |

The closing summary lists what was set up under **Security**.

Docker and ufw: Docker inserts its own forwarding rules ahead of ufw's, so
containers keep reaching each other and the internet with ufw's routed policy
left at deny; and since Modula's containers publish no ports, Docker's rules
can't open anything past the firewall. Ports you publish yourself with
`docker run -p` would bypass ufw, as on any Docker host.

A cloud firewall in front of the server (Hetzner, AWS security groups) needs
the same ports open.

### Users

| Service | Runs as | Why |
|---|---|---|
| `modula-server` | `modula` (system user, no shell) | Control plane; peer-authenticated to Postgres as the `modula` role. |
| `modula-caddy` | `modula-caddy` (system user, no shell) | Binds 80/443 with `CAP_NET_BIND_SERVICE` only. |
| `modula-node` | `root` | Drives Docker and Caddy's admin socket, which is root-equivalent anyway. |

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
curl -fsSL https://usemodula.org/install.sh \
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
