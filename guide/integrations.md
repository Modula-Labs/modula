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

1. An instance admin creates the App once through GitHub's manifest flow
   (`POST /api/github/app/manifest`, sudo; the browser then posts the returned
   manifest to GitHub, which redirects back to `/api/github/app/callback`).
   Its credentials are stored encrypted. `GET /api/github/app` shows whether
   it exists.
2. A workspace admin installs it on the GitHub accounts or organizations whose
   repositories the workspace should use
   (`POST /api/workspaces/{id}/github/install` returns the GitHub URL to open).
3. In **New service → GitHub repo**, enter `owner/name`, the branch and an
   optional root directory. `GET /api/workspaces/{id}/github/repos` lists the
   repositories the installation can reach.

Dashboard screens for steps 1 and 2 are coming soon.

Pushes arrive on `POST /api/github/webhook`, verified by HMAC signature. A push
deploys every service instance whose `source.branch` matches, has
`autoDeploy` on, and whose `watchPaths` (if any) match a changed file.
Deployments report their status on the commit in GitHub.

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

Not yet: locking the origin to Cloudflare's IP ranges, Authenticated Origin
Pulls, Cloudflare Tunnel ingress, and wildcard custom domains (DNS-01
certificates).

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
   only 80/tcp, 443/tcp and 443/udp (plus 22/tcp with `"ssh": true`).
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

The Hetzner firewall doesn't open the TCP proxy range (20000–29999), so
[TCP proxies](databases.md#public-access-tcp-proxy) on these servers aren't
reachable unless you open it in the Hetzner console.
