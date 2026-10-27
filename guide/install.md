# Install

The main server runs the dashboard and API (`modula-server`), its Postgres and
Redis, and is also the first server your apps run on (`modula-node`, Docker,
Caddy). More servers can [join](servers.md) later.

## Requirements

- A fresh **Ubuntu 22.04+** or **Debian 12+** server with systemd, `amd64` or
  `arm64`, and root or `sudo` access.
- Optional: a hostname for the dashboard, e.g. `deploy.example.com`, with an
  `A` record pointing at the server. Without one the dashboard is served at
  `https://<public IP>` and you can add a domain later in **Settings → Domain**.
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
| 20000–29999 (TCP) | Only if you enable [TCP proxies](databases.md#public-access-tcp-proxy) for databases. `modula-node` opens each allocated port in ufw while it's in use; a cloud firewall in front of the server needs the range (or the ports you use) opened by you. |

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

- optionally the **dashboard domain**, e.g. `deploy.example.com` (it shows this
  server's public IP for the `A` record and warns if the domain doesn't point
  here yet); leave it empty to use `https://<public IP>`,
- an **email** for TLS certificates (Let's Encrypt),
- a **server name** for the Servers page (default: the short hostname),

then confirms before changing anything. Not running as root? It asks `sudo`
to run itself again as root (fetching the script once more from
`https://usemodula.org/install.sh`, since a piped script has no file).

For automation (cloud-init, CI) pass the answers as flags; with `--email` it
asks nothing:

```bash
curl -fsSL https://usemodula.org/install.sh \
  | sudo bash -s -- --domain deploy.example.com --email you@example.com --apps-domain apps.example.com
```

| Option | Description |
|---|---|
| `--domain HOST` | Dashboard hostname. DNS must point here. Without it the dashboard is on `https://<public IP>`. |
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
| Docker restarts | `"live-restore": true` in `/etc/docker/daemon.json`, so containers keep running when Docker itself restarts (a security update of `docker.io`). Only when the file is absent or the installer wrote it (never into a file you manage, never on swarm nodes); applied with a reload, which leaves containers alone. Upgrades add it too, under the same rule. |
| Swap | With less than 2 GiB of memory and no swap, a 2 GiB `/swapfile` (in `/etc/fstab`) and `vm.swappiness = 10`. |
| Clock | Makes sure systemd-timesyncd (or chrony/ntp, if present) keeps the clock in sync; certificates and mutual TLS depend on it. |
| Kernel | `/etc/sysctl.d/90-modula.conf`: `net.core.somaxconn = 4096`, `fs.inotify.max_user_watches = 524288`, `fs.inotify.max_user_instances = 1024`, `net.ipv4.ip_forward = 1` (containers reach the internet through the host). |
| Cloud metadata | `modula-firewall.service` adds `DOCKER-USER` rules that drop container and build traffic to `169.254.169.254` and `fd00:ec2::254`, so apps can't read the server's cloud-init data or cloud credentials. `modula-node` re-asserts them when it starts. The host itself can still reach the metadata service. |
| Memory pressure | `OOMScoreAdjust=-500` for `modula-server`, `modula-node`, Postgres and Redis, so the kernel kills an app container before the platform. |
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
| `modula-node` | `root` (`PrivateTmp`, `ProtectHome`, `NoNewPrivileges`) | Drives Docker, the firewall and Caddy's admin socket, which is root-equivalent anyway. |
| Railpack (build planning) | `modula-build` (system user, no shell) | Reads untrusted repositories, so it doesn't run as root. Builds themselves run in a rootless, sandboxed BuildKit per workspace. |

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
| `/var/lib/modula-upgrade` | Upgrade requests and the last upgrade's report (see [Updates](#updates)) |
| `/etc/modula/upgrade.env` | Where `modula-upgrade` fetches the installer from |

Services: `modula-server`, `modula-node`, `modula-caddy` (systemd), with
`modula-server.socket` and `modula-caddy.socket` holding their ports, and
`modula-upgrade.path` / `modula-upgrade.service` for upgrades started from the
dashboard.

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
| `MODULA_RELEASE_URL` | Where releases are published, for update checks (default `https://downloads.usemodula.org/releases`). |
| `MODULA_RELEASE_PUBKEY` | The release signing key (base64 SubjectPublicKeyInfo, default: the one the installer embeds). |
| `MODULA_UPGRADE_DIR` | Where upgrade requests go (default `/var/lib/modula-upgrade`). |

## Upgrade

The easy way is the dashboard: **Settings → Updates → Update now** (instance
admins, re-authentication required). From your laptop, `modula server update`
shows the versions and follows the upgrade (see the [CLI guide](cli.md#server-update---check)).
The same from a shell, on the main server:

```bash
curl -fsSL https://usemodula.org/install.sh \
  | sudo bash -s -- upgrade [--version X.Y.Z]
```

Either way the release is downloaded, its signature checked against
`/etc/modula/release.pub.pem` (the signed `VERSION` must match the version
asked for), and switched atomically. Downgrades are refused unless you set
`MODULA_ALLOW_DOWNGRADE=1`. Upgrades started from the dashboard also verify the
installer script itself against the signed checksums before running it; if the new control plane doesn't become healthy within a minute,
the previous release is switched back and restarted (the installer exits with
status 3). Running apps keep running throughout.

Once the main server runs the new version, it upgrades every other server to
the same version, **one at a time**: it asks a server over its connection, the
server's `modula-upgrade` unit runs the installer from your instance, and the
next server is asked only after this one reconnected on the new version. A
failure stops the rollout (the Updates section shows which server and why)
until you press **Update now** again. Servers on releases from before this
existed show as *Manual*; upgrade those once by hand, from your instance:

```bash
curl -fsSL https://deploy.example.com/api/install.sh | sudo bash -s -- upgrade
```

An older server keeps working with a newer control plane in the meantime.

### What an upgrade interrupts

| Part | During an upgrade |
|---|---|
| Your apps' containers | Keep running; they are never restarted by an upgrade. |
| Apps' HTTP(S) traffic (Caddy) | Not interrupted. Caddy restarts only when the release ships a different Caddy; even then systemd keeps ports 80/443 open (`modula-caddy.socket`), new connections wait a moment in the kernel's queue, and requests in flight get 10 s to finish. |
| Dashboard and API (`modula-server`) | Not interrupted: `modula-server.socket` keeps `127.0.0.1:7100` and `7101` open while the process is replaced. Requests in flight get up to 20 s to finish; open log streams end with a `restart` event and the dashboard reconnects. |
| Servers' connections to the control plane | Each server's stream drops when the control plane restarts and reconnects within 1–8 s (backoff); workloads aren't affected. |
| TCP proxies (ports 20000–29999) and `modula connect` tunnels | Live in `modula-node`: connections through them drop when that server's node restarts, and new ones are refused for the 1–3 s it takes to come back. Database drivers reconnect; reconnect long-lived clients. |

The very first upgrade to a release with socket activation moves the ports
from the processes to systemd, which takes one restart of each with a gap of
about a second; every upgrade after that is seamless as described.

## Updates

**Settings → Updates** (instance admins) shows this instance's version, the
newest release, and every server's version.

| Mode | What happens |
|---|---|
| Off | No checks. |
| Notify only (default) | Checks once a day (`VERSION` from the release URL, trusted only with a valid signature over `SHA256SUMS`) and shows *Update available* in the dashboard. The audit log records each new version once. |
| Automatic | Also upgrades in the **maintenance window**: the weekdays, start time and time zone you pick (default Sunday 03:00, the server's own time zone), for an hour. A version that failed or rolled back isn't tried again automatically. |

Changing the mode or window and **Update now** need re-authentication; all
of it is in the audit log, with each upgrade's result and the tail of the
installer's output.

`modula-server` runs as the unprivileged `modula` user and can't upgrade
anything itself. It writes the requested version to
`/var/lib/modula-upgrade/request`; the root-owned `modula-upgrade.path` unit
runs the installer's `upgrade` for that version, which must be a release no
older than the installed one, and writes the outcome to
`/var/lib/modula-upgrade/result`. Follow along with
`journalctl -u modula-upgrade -f`.
