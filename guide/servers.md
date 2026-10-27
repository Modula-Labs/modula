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

A service's first replica runs on one server, chosen on its first deployment
(or when a volume or TCP proxy is created). In order of preference:

1. an online server;
2. not the main server, when another one is usable (see
   [Keep apps off the main server](#keep-apps-off-the-main-server));
3. the closest to the services it references through variables
   (`${{postgres.DATABASE_URL}}` puts an app next to its database), or, without
   references, to where most of the environment's services run (distance
   between the servers' [locations](#locations));
4. the fewest running deployments.

After that the service stays on that server for every deployment, until you
move it (**Settings → Regions & Replicas**) or the server is removed. The
server picker lists the recommended server first, the same one the first
deployment would pick (`GET /api/environments/{id}/placement-suggestion`).

Servers added by an instance admin (the join command, or `modula-server
join-token`) are shared by every workspace. Servers a workspace creates through
its own [Hetzner integration](#create-servers-in-hetzner) belong to that
workspace: only its services are placed on them.

## Keep apps off the main server

The main server runs the control plane: the dashboard, the API, its Postgres
database with every project's settings, the instance's master key and
certificate authority, and the tunnels to every other server. An app running
next to it shares its kernel, CPU, memory and disk. With more than one server,
run apps on the others:

- **Security isolation.** Containers are sandboxed, but a container escape or
  a kernel bug on the main server reaches the master key, every sealed variable
  and the CA that every other server trusts. On a worker it reaches that
  worker's apps only.
- **Noisy neighbours.** An app that spikes CPU, leaks memory or fills the disk
  slows down or stops the dashboard and its database, and with them deploys,
  logs and health checks for every server.
- **Blast radius.** When the main server is in trouble, so is everything it
  manages. Keeping it lightly loaded keeps the rest of the fleet manageable
  while you fix an app.
- **Upgrades and migrations.** Updating, resizing or migrating the main server
  restarts the control plane; apps on it go down with it, while apps on other
  servers keep serving.

New services already avoid the main server when another one is usable (see
[Placement](#placement)). Services that run there show a warning on the
project page; it opens the service's server picker with the recommended server
selected, so moving it is one confirmation. A single-server setup (a hobby
project, a trial) is fine: the warning stays a note until you add a server.

## Server health

The project page's **Warnings** list a service whose server is offline (it may
be down and can't be deployed until the server reconnects: check the server's
page, its network and `systemctl status modula-node`) and a server whose disk
is more than 90% full (builds, logs and volumes fail when it fills: remove
unused images and volumes, or resize the server).

## Migrate a server

**Servers → a server → Migrate** creates a new server at the same provider
(another location or size), waits for it to join, and moves every service on
the old one to it, volume data included (a service with a volume is down
while its data copies). The old server is then marked for deletion; delete
it from Servers when you're happy. Abort moves everything back.

The **main server** (the one running the dashboard) can be migrated the same
way; the control plane moves with it:

1. Services move first, as above.
2. The dashboard goes read-only for a few minutes ("Moving to the new
   server…"; changes get 503). Apps keep running.
3. The new server downloads a copy of the control plane over its own node
   connection (database, master key, uploads) and installs it
   (`install.sh main --restore`, run by modula-node).
4. Once its node endpoint answers with this instance's CA, the hand-over
   starts: the new server's node switches to its local control plane, every
   other server is told the new address (they check it works before
   switching), and the dashboard's DNS records Modula manages in a connected
   Cloudflare zone are pointed at the new IP. Anything else (the progress
   view lists it) you point there yourself.
5. The old dashboard then redirects to the new one and sends late servers
   on. The new control plane revokes the old server's node and marks it for
   deletion.

Things to know:

- If the dashboard is reached by IP, its address changes to the new
  server's IP: bookmarks change, and CLI users run `modula login <new address>`.
- DNS caches may keep the old address for a few minutes.
- Turn private mode off first (and on again afterwards).
- Until the hand-over starts, a failure ends the maintenance and abort moves
  the services back (the half-installed control plane on the new server does
  nothing; delete that server). From the hand-over on there is no automatic
  way back; the old server stays until you delete it.
- The main server must be linked to its provider server (found by its IP in
  a connected Hetzner project), and every server should run the current
  modula-node (older ones can't be told the new address: re-join them).

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
