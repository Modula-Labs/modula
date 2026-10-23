# Security

## Two-factor authentication

**Account → Security → Two-factor authentication** enrolls an authenticator
app (TOTP) and gives you single-use recovery codes. With 2FA on, signing in
asks for a code after your password (5 wrong codes end the attempt), and
re-authentication (below) asks for a code instead of your password. Disabling
2FA, regenerating recovery codes and changing your password need sudo.
Changing your password signs out every other session.

**Account → Security → Sessions** lists where you're signed in; revoke any
session you don't recognize. Sessions expire after 7 days idle and 30 days in
total.

## Sudo mode

Destructive and secret-revealing actions ask you to confirm it's you (your
2FA code, or your password without 2FA). That unlocks them for 15 minutes.

Actions that need sudo:

- delete a workspace, project, environment, service or volume
- reveal a variable's value
- restore or delete a backup, delete a backup target, change the control-plane backup
- rotate a database password
- create a server join token, remove a server, create the GitHub App
- connect, change or disconnect Cloudflare or Hetzner; create or delete a
  Hetzner server
- create an API token, change 2FA, regenerate recovery codes, change your password

**API tokens never get sudo**, so none of these can be scripted or done by an
AI agent with a token. That's deliberate.

## API tokens

API tokens (`mdt_…`) authenticate the CLI and scripts as you, with your roles.

- **Create:** `POST /api/account/tokens` (sudo) with
  `{ "name": "laptop", "workspaceId": "<id or null>", "expiresInDays": 90 }`.
  The token is shown once; only its hash is stored. Limit it to one workspace
  when you can (`workspaceId`), and give it an expiry (1–3650 days, or `null`
  for none). At most 50 tokens per user.
- **List:** `GET /api/account/tokens` (name, workspace, last used, expiry).
- **Revoke:** `DELETE /api/account/tokens/{token_id}`.

The dashboard page for tokens is coming soon. Until then, create one from
your browser's developer console while signed in to the dashboard (same
origin, so your session cookie is used):

```js
const h = { "content-type": "application/json" };
// 1. Re-authenticate. With 2FA on, only a code is accepted; without 2FA, only your password.
const code = prompt("2FA code (leave empty if you don't use 2FA)");
await fetch("/api/auth/sudo", { method: "POST", headers: h,
  body: JSON.stringify(code ? { totpCode: code } : { password: prompt("Password") }) });
// 2. Create the token (shown once)
const r = await fetch("/api/account/tokens", { method: "POST", headers: h,
  body: JSON.stringify({ name: "laptop", workspaceId: null, expiresInDays: 90 }) });
(await r.json()).token
```

Treat a token like a password: keep it in a secret store or `MODULA_TOKEN`,
never in a repository, a chat or a log.

## Roles

| Role | Can |
|---|---|
| **Viewer** | See projects, settings, variable names, deployments, logs, backups. |
| **Deployer** | + set variables, deploy, redeploy, roll back, restart, remove deployments, back up now. |
| **Member** | + create projects, environments and services; change settings; domains, volumes, TCP proxies; open tunnels; the Database tab; reveal variables (sudo); restore/delete backups (sudo). |
| **Admin** | + workspace settings, delete projects/environments/volumes (sudo), backup targets, GitHub installation. |

Instance administrators manage servers, join tokens, the GitHub App and the
control-plane backup. Being an instance admin grants no access to workspaces.

## Secrets

- Variable values are encrypted with a key per environment, which is wrapped
  by the instance's master key (`/var/lib/modula/master.key`). Keep the
  [recovery kit](install.md#recovery-kit) offline.
- Values are write-only over the API. Revealing one needs sudo and is audited.
  The exception is config as code: `GET /api/environments/{id}/config`
  (Member, so API tokens can run `modula config plan/pull`) returns
  the values of non-sealed variables of one environment, and each call is
  audited (`environment.config_read`) with the names it returned. Sealed
  values are never returned. Database credentials are likewise readable by
  Members.
- **Sealed variables** can never be revealed by anyone and are never copied to
  other environments. See [variables](variables.md#sealed-variables).
- Deploy logs mask variable values (and their base64 and URL-encoded forms).
  Values shorter than 6 characters aren't masked.

## Audit log

Every mutation and security event (sign-ins, sudo, variable updates and
reveals, deployments, domain changes, database queries, backups, token
creation…) is recorded in an append-only audit log with the actor, IP and
target. Variable values and query results are never logged. A dashboard view
of the audit log is coming soon.

## Containers and servers

- Containers run with all Linux capabilities dropped except a small allowlist,
  `no-new-privileges`, an init process and memory/CPU/PID limits. There is no
  "privileged" setting.
- Each environment has its own network; nothing is published on the host
  except through the edge (Caddy) and TCP proxies you create.
- Servers join with a single-use, one-hour join token and pin the instance's
  CA fingerprint; removing a server cuts off its certificate immediately.
