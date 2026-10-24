# Servers

The main server runs the dashboard and is also your first server for apps.
Add more servers to run more services. Each one runs `modula-node`, Docker and
Caddy, and connects out to the main server over mutual TLS; it needs no
inbound port for the dashboard.

Servers are managed by **instance admins** under **Servers**.

## Add a server

Requirements are the same as for the [main server](install.md#requirements):
Ubuntu 22.04+ or Debian 12+, amd64 or arm64, systemd, root. Open ports 80 and
443 for apps (and 20000–29999 if it will host TCP proxies) in any cloud
firewall; the installer sets up the server's own firewall the same way (see
[host setup](install.md#host-setup); `--no-harden` skips it). The server must
reach the main server on port 7101.

1. **Servers → Add server** (asks you to re-authenticate). It shows a one-line
   command with a single-use join token, valid for 1 hour.
2. Run it on the new server:

   ```bash
   curl -fsSL https://deploy.example.com/api/install.sh \
     | sudo bash -s -- join --server https://deploy.example.com:7101 --token mcj_… --ca sha256:…
   ```

   | Option | Description |
   |---|---|
   | `--server URL` | The instance's node endpoint (`https://…:7101`). |
   | `--token TOKEN` | The join token (`mcj_…`). |
   | `--ca FINGERPRINT` | The instance's CA fingerprint (`sha256:<64 hex>`), pinned so the server can't be tricked into joining another instance. |
   | `--name NAME` | Display name (default: the hostname). Letters, digits, `. _ -`. |

3. The server appears under **Servers** within seconds.

The installer comes from your instance, so the new server runs the same
version as the main server. The new server generates its own key; only a
certificate request leaves it.

The dialog also offers the join step alone, for a server that already runs
`modula-node`:

```bash
modula-node join --server https://deploy.example.com:7101 --token mcj_… --ca sha256:… [--name NAME]
```

Join tokens can't be created with an API token (they need sudo).

## Locations

Each server's location (city/region) is detected automatically. An admin can
pin it on the server's page (`PATCH /api/nodes/{id}` with
`{ "location": "<id>" }`, or `null` to go back to detection). The list of
locations is `GET /api/locations`.

## Placement

A service instance runs on one server (`replicas` is 1 for now). The server is
chosen on its first deployment (or when a TCP proxy is created): an online
server with the fewest running deployments. After that the service stays on
that server for every deployment, until the server is removed.

Servers added by an instance admin (the join command, or `modula-server
join-token`) are shared by every workspace. Servers a workspace creates through
its own [Hetzner integration](#create-servers-in-hetzner) belong to that
workspace: only its services are placed on them.

## Remove a server

**Remove** on the server's page revokes its certificate: it's cut off
immediately, its deployments stop being managed, and it can never reconnect
with its old certificate. To bring it back, join it again with a new token.
Needs re-authentication.

A server renews its certificate on its own during the last third of the
certificate's 30-day lifetime. A server that floods the control plane (more
than about 200 log, metric or event messages per second, or 4 MiB/s of logs)
has the excess dropped; log lines longer than 16 KiB are cut.

## Builds

Each workspace's builds run in their own rootless BuildKit daemon on the
server, with its own build cache, the process sandbox on, a seccomp profile,
and limits of 2 GiB memory and 2 CPUs. Build steps can't reach environment
networks or the cloud metadata service. At most 2 builds run at once per
server; more wait for a slot. A workspace's daemon stops after 30 idle minutes
(its cache stays), so the next build starts a few seconds slower. Tune with
`modula-node` flags or environment: `--build-memory-mb` / `MODULA_BUILD_MEMORY_MB`,
`--build-cpus` / `MODULA_BUILD_CPUS`, `--build-concurrency` /
`MODULA_BUILD_CONCURRENCY`.

## Create servers in Hetzner

With [Hetzner connected](integrations.md#hetzner) to a workspace, workspace
admins can create a server in a chosen location and type; it boots, installs
everything and joins on its own within a few minutes (single-use 30-minute
join token, CA pinned, firewall open on 80/443 only). Spending caps (servers
and euros per month) apply, and servers are only created when you ask. See
[integrations → Hetzner](integrations.md#hetzner).

Without the integration, create the server in Hetzner (Ubuntu 24.04 or
Debian 12) and use the join command above.
