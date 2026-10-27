# Integrations

Integrations are connected per workspace. Their tokens are verified, stored
encrypted with the workspace's key, and never returned by the API (Cloudflare
shows the last four characters). Connecting and disconnecting needs the
**Admin** role and a recent re-authentication. Dashboard screens for
connecting Cloudflare and Hetzner are coming soon; today they're connected
through the API from a dashboard session (like [creating an API
token](security.md#api-tokens)).

## GitHub

Deploying from repositories uses a GitHub App that belongs to your instance.
Connecting it is one click: **Connect GitHub**, in **Workspace Settings →
Integrations → GitHub** or under the Repository field of **New service →
GitHub repo** (workspace admins).

- **First time on the instance** (instance admins, with a recent
  re-authentication): GitHub shows the App it is about to create; confirm it.
  The App is named `Modula Deploy (<your host>)` (with a short suffix when
  another instance already took that name; GitHub lets you edit it on that
  page too), belongs to your GitHub account, asks only for **contents** and
  **metadata** (read) and **commit statuses** (write), and sends **push**
  events to `/api/github/webhook` on the instance's public URL. GitHub hands
  its keys back to the server, which stores them encrypted, and goes straight
  on to installing it. Nothing to type: the App is *public* (installable on
  any account), so one App covers your personal account and every
  organization, and GitHub's installation page lists them to pick from.
  Under **Advanced** you can have an organization own the App instead (you
  must be an owner of it), for example so it doesn't depend on one person's
  account.
- **Every time**: pick the account or organization and the repositories on
  GitHub's installation page. You land back where you started: in Settings,
  or in the New Service dialog on the GitHub step with the repository search
  ready and "GitHub connected — pick a repo". **Install on another account**
  (Settings, or the account menu of the Repository field) adds more accounts
  and organizations the same way.
- **Organization approval**: if an organization owner has to approve the
  installation, you're told so. Once they approve it on GitHub, click
  **Connect GitHub** again to finish binding it to the workspace.
- Workspace admins on an instance without an App are asked to get an instance
  admin to connect GitHub; other roles are asked to get a workspace admin.

Installing also asks you to authorize the App on GitHub (OAuth during
installation). The server uses that one-time authorization only to check that
the installation you came back with is one *you* can access on GitHub, then
discards it, so nobody can bind someone else's installation by guessing its id.
An App created before this check existed must have **Request user authorization
(OAuth) during installation** turned on in its GitHub settings, with the
callback URL `https://<your instance>/api/github/install/callback`; or delete
it and connect GitHub again.

The installation is bound to the workspace that started it and can't be moved
to another. Being public, the App can also be installed by people who have
nothing to do with your instance; that gives them nothing. An installation is
used only once it's bound to a workspace, and it binds only by coming back
through **Connect GitHub** on this instance (signed in as a workspace admin,
with the `state` below and the OAuth check above). Webhooks for installations
no workspace has bound are acknowledged and dropped without being recorded.
Apps created by earlier versions are private and keep working: each installs
only on the account that owns it, and another account needs another App
(**Add GitHub App** in Settings creates a public one).

Installed accounts show with their avatar and a **Configure** link
to change repository access on GitHub. Every hop through GitHub carries a
single-use `state` (stored hashed, bound to the user who started it, and
expiring after 15 minutes for App creation, an hour for installing), so a
link can't be replayed or completed by someone else; the return path must be
a path on this instance.

In **New service → GitHub repo**, the Repository field searches the
repositories your installations reach (most recently pushed first, private
ones marked with a lock). Picking one fills the branch with its default
branch and the service name, and the Branch field then suggests branches.
`owner/name` or a GitHub URL of any public repository works without
connecting anything.

Listing repositories and branches needs the **Member** role. The listings are
cached for a minute per workspace and GitHub calls are rate limited, so typing
in the pickers doesn't spend GitHub's API quota. Installation tokens never
leave the server.

| Endpoint | Who | What |
|---|---|---|
| `GET /api/github/app` | Signed in | Whether the App exists, its slug, owner and webhook URL |
| `POST /api/github/app/manifest` | Instance admin, sudo | Manifest and GitHub URL to post it to. With `workspaceId` (an admin of it) the callback goes on to installing it there; `returnTo` is the path to come back to |
| `POST /api/workspaces/{id}/github/install` | Admin | The GitHub URL that installs the App for this workspace; `returnTo` as above |
| `GET /api/workspaces/{id}/github/installations` | Member | Installed accounts (`accountLogin`, `avatarUrl`, `configureUrl`) |
| `GET /api/workspaces/{id}/github/repos?q=&page=&perPage=` | Member | `fullName`, `private`, `defaultBranch`, `updatedAt` (last push), `ownerAvatar` |
| `GET /api/workspaces/{id}/github/branches?repo=owner/name&q=` | Member | `name`, `protected` |

Pushes arrive on `POST /api/github/webhook`, verified by HMAC signature and
rate limited per source IP. A push deploys every service instance whose
`source.branch` matches, has `autoDeploy` on, and whose `watchPaths` (if any)
match a changed file. A push for a commit that service already has a
deployment of is ignored, so a replayed webhook can't roll a service back; to
redeploy an old commit, use **Rollback** or **Redeploy**.
Deployments report their status on the commit in GitHub. Creating the App,
installing, uninstalling and changes to an installation's repositories are
recorded in the audit log.

## Cloudflare

Optional. With Cloudflare connected to a workspace:

- **DNS records for custom domains are written for you** when the domain's
  zone is in the connected account: the CNAME/A routing record and the TXT
  ownership record. They're removed when you delete the domain.
- **Proxied or DNS only** can be switched per custom domain, and the zone's
  SSL/TLS mode is checked (it must be Full (strict) for proxied domains).
- **R2 backups** in one step: the bucket is created for you and added as a
  backup target.

### Token permissions

Create a Cloudflare **user API token** with:

| Permission | Why | Required |
|---|---|---|
| Zone · Zone · Read | Find the zone a domain lives in | Yes |
| Zone · DNS · Edit | Create and remove records for your domains | Yes |
| Zone · Zone Settings · Read | Warn when the SSL mode breaks proxied domains | Yes |
| Account · Workers R2 Storage · Edit | Create backup buckets | Only for R2 |
| Account · Cloudflare Tunnel · Edit | Run the dashboard through a tunnel | Only for private mode |
| Account · Access: Apps and Policies · Edit | Put Cloudflare Access in front of the dashboard (and create its service token) | Only for private mode |

Limit **Zone Resources** to the zones you'll use, and **Account Resources** to
your account. `GET /api/workspaces/{id}/integrations/cloudflare` returns
`tokenTemplateUrl`: a link to Cloudflare's token page with exactly these
permissions pre-filled.

### Connect

```js
// From the dashboard's browser console, after re-authenticating (see security.md)
const ws = "<workspace id>";
const h = { "content-type": "application/json" };
const token = prompt("Cloudflare token");
// 1. Preview: accounts and zones the token can reach (stores nothing)
await (await fetch(`/api/workspaces/${ws}/integrations/cloudflare/verify`, { method: "POST", headers: h,
  body: JSON.stringify({ token }) })).json();
// 2. Connect (pass accountId when the token sees several accounts)
await fetch(`/api/workspaces/${ws}/integrations/cloudflare`, { method: "PUT", headers: h,
  body: JSON.stringify({ token, accountId: null }) });
```

`DELETE …/integrations/cloudflare` forgets the token; DNS records already
written stay at Cloudflare.

### Custom domains with Cloudflare

When you add a custom domain whose zone the token manages, its records are
created **DNS only**. Then:

1. Wait for the domain to verify and serve HTTPS (see [domains](domains.md)).
2. Switch it to proxied: `PUT /api/domains/{id}/ingress` with
   `{ "proxied": true }` (rewrites the CNAME/A record as proxied; the TXT
   record is never proxied). `GET /api/domains/{id}/ingress` returns
   `managed`, `zone`, `proxied`, `sslMode` and an `sslWarning` when the zone's
   mode isn't **Full (strict)**.
3. Set the zone's **SSL/TLS** mode to **Full (strict)** if the warning says so.

Not yet: locking app origins to Cloudflare's IP ranges, Authenticated Origin
Pulls, Cloudflare Tunnel ingress for apps, and wildcard custom domains (DNS-01
certificates). The dashboard itself can go through a tunnel: see below.

### R2 backups

`GET …/integrations/cloudflare/r2` returns your account's R2 `endpoint`,
whether the token can create buckets, and `apiTokensUrl`, where you create an
R2 **S3 access key pair** (Cloudflare's API can't). Then:

```bash
POST /api/workspaces/{id}/integrations/cloudflare/r2-targets
{ "name": "r2", "bucket": "modula-backups", "prefix": "", "accessKeyId": "…", "secretAccessKey": "…" }
```

creates the bucket if it's missing and adds a normal `r2` backup target (Admin).
You can also add R2 by hand without connecting Cloudflare; see
[backups](backups.md#1-add-a-backup-target).

### Private dashboard (Cloudflare Tunnel)

One switch hides the dashboard, the API, the CLI endpoint and the servers'
node endpoint from the public internet behind **Cloudflare Tunnel** and
**Cloudflare Access**. Apps stay public on their own domains (Caddy keeps
serving them on 80/443).

**Settings → Private dashboard** (instance admin, workspace admin, sudo) shows
a readiness checklist, who may sign in (default: the workspace's admins; one
email or `@domain` per line) and the server hostname. It uses the workspace's
Cloudflare connection, whose token needs the two "private mode" permissions
above; the checklist asks Cloudflare (a harmless list call) and tells you to
replace the token when they are missing. Zero Trust must be set up once in
the account (one.dash.cloudflare.com, pick a team name; the free plan is
enough).

Turning it on, step by step (the dashboard shows each one):

1. A service token **Modula servers** and a tunnel `modula-<host>`, configured
   at Cloudflare: `<dashboard host> → http://127.0.0.1:7100`,
   `<server host> → tcp://127.0.0.1:7101`, anything else 404.
2. Three Access applications: the dashboard (your emails/domains, plus *any
   service token* for the CLI and CI), a **bypass** for the endpoints that
   authenticate themselves (`/api/install.sh`, `/api/releases`,
   `/api/build-source`, `/api/github/webhook`), and the server hostname (only
   the Modula service token).
3. A proxied CNAME for the server hostname (default `<first label>-nodes.<rest>`,
   e.g. `deploy-nodes.example.com`, which Cloudflare's free certificate covers).
4. The main server installs `cloudflared` and runs the tunnel
   (`modula-cloudflared.service`); Modula waits until Cloudflare reports it
   healthy.
5. Every other server moves its connection to a local
   `cloudflared access tcp` listener on 127.0.0.1:7101
   (`modula-cloudflared-access.service`) after a connection through it
   worked. All servers must be online and up to date, or enabling is refused.
6. The dashboard's DNS records are replaced by a proxied CNAME to the tunnel
   (the old records are kept and restored when you turn it off), and Modula
   waits until the dashboard answers with the Access sign-in.
7. Only then: every request must carry a valid `Cf-Access-Jwt-Assertion`
   (signature against the team's keys, audience, issuer, expiry), the node
   endpoint accepts only connections from the tunnel, and the main server
   stops serving the dashboard on its public edge and closes port 7101 in ufw.

If a step fails, everything done so far is undone and the error is shown.
Turning it off reverses it all: the edge serves the dashboard again, DNS is
restored, servers dial directly, cloudflared stops, and the tunnel, Access
apps and service token are deleted.

**cloudflared** is Cloudflare's `.deb` from its GitHub release, pinned to one
version and checked against the SHA-256 sums from the release notes before
`dpkg -i` (an existing `/usr/bin/cloudflared` is used as is).

**New servers** in private mode get a join command that dials
`https://127.0.0.1:7101` and carries `--access-host/--access-id/--access-secret`:
`modula-node join` sets up the access listener before enrolling. The command
contains the servers' service token; treat it like the join token.

**CLI and CI**: `modula login` notices the Access sign-in. With
[cloudflared](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/)
installed it opens your browser (`cloudflared access login`) and saves the
app token next to the API token (mode 0600), sent as `cf-access-token` and
renewed through `cloudflared access token` when it expires. For CI, create a
service token in Zero Trust (Access → Service credentials) and set
`MODULA_CF_ACCESS_CLIENT_ID` and `MODULA_CF_ACCESS_CLIENT_SECRET`; both are
sent on every request, tunnels (`modula connect`) included.

**Break glass** when Cloudflare is unreachable: `ssh -L 7100:127.0.0.1:7100
<main server>`, then open http://localhost:7100. Direct local connections are
the only ones allowed without Access.

**Stranded servers**: a server that was offline while private mode was
turned off still dials the tunnel. On it: set `"server"` in
`/var/lib/modula-node/node.json` back to `https://<dashboard host>:7101`, run
`systemctl disable --now modula-cloudflared-access` and restart
`modula-node`.

## Hetzner

Optional. With a Hetzner Cloud token connected, workspace admins can create
servers in Hetzner from Modula Deploy. Servers are only ever created when you
ask; Modula Deploy only *suggests* adding one when a service doesn't fit.

### Connect

Create a token in the Hetzner Cloud console (**your project → Security → API
tokens**) with **Read & Write** permission, then (Admin + sudo):

```bash
PUT /api/workspaces/{id}/integrations/hetzner      { "token": "…" }
```

The token is checked by listing locations. Spending caps (defaults: 5
servers, €100.00 per month, gross) protect you from mistakes:

```bash
PATCH /api/workspaces/{id}/integrations/hetzner    { "maxServers": 10, "maxMonthlyCents": 20000 }
```

`maxServers` is 0–100. `GET …/integrations/hetzner` shows the caps and current
usage. Disconnecting is refused while servers created through it exist.

### Create a server

1. Pick a location and type: `GET …/integrations/hetzner/catalog` lists
   locations (ids like `hetzner:fsn1`) and orderable server types with gross
   monthly prices; `GET …/integrations/hetzner/ssh-keys` lists your Hetzner
   SSH keys.
2. Create it (Admin + sudo):

   ```bash
   POST /api/workspaces/{id}/servers/provision
   { "location": "hetzner:fsn1", "serverType": "cx22", "name": "worker-1", "sshKeyIds": [], "ssh": false }
   ```

   The caps are checked first. The server boots Ubuntu 24.04 with cloud-init
   that installs Docker, Caddy and `modula-node` and joins your instance with a
   single-use, 30-minute join token and the CA pin. A Hetzner firewall allows
   only 80/tcp, 443/tcp, 443/udp, the TCP proxy range 20000–29999/tcp and ping
   (plus 22/tcp with `"ssh": true`); the installer also sets up ufw on the
   server itself ([host setup](install.md#host-setup)).
3. Follow it: `GET /api/servers/{id}` → `status` goes `creating` → `booting`
   → `joining` → `ready` (or `failed`, with `error`). It then shows up under
   **Servers**.

`GET /api/workspaces/{id}/servers` lists servers created this way with caps
and usage. `GET /api/workspaces/{id}/servers/suggest?cpuMillis=&memoryMb=&location=`
(Member) answers "where does this service fit?": existing servers with room,
or the cheapest server type that fits.

### Delete a server

`DELETE /api/servers/{id}` (Admin + sudo) deletes it at Hetzner and removes it
from the instance. It's refused while services run on it; move them first, or
pass `?force=true`.

The Hetzner firewall opens the TCP proxy range (20000–29999), so
[TCP proxies](databases.md#public-access-tcp-proxy) on these servers are
reachable without changes in the Hetzner console.
