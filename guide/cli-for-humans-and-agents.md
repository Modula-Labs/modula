# The `modula` CLI, for people and AI agents

`modula` drives Modula Deploy from a terminal, a CI job or an AI agent. This
guide gets you from nothing to a deployed app; `modula docs` adds the full
command reference, and `modula <command> --help` has every flag.

Everything here works two ways: **interactively** (a terminal: you're asked
for what's missing) and **non-interactively** (no terminal, or `--json`:
nothing is ever asked, a missing answer fails at once with exit code 2 and a
list of what to pass). Agents should always use the second way.

## Install

```bash
curl -fsSL https://usemodula.org/install-cli.sh | bash
```

It installs `modula` to `~/.local/bin` (`--dir` to change it, `--version
X.Y.Z` for a specific release), checks the download against the release
signature, and then asks what's next: connect to an existing server or create
a new one. Without a terminal it just prints both commands.

From source: `cargo install --path crates/cli`.

## Create a server on Hetzner

You need a Hetzner Cloud account and an API token with **Read & Write**
(console.hetzner.cloud → your project → Security → API tokens), a domain for
the dashboard, and an email address.

```bash
modula server create
```

The wizard asks, one step at a time (Esc goes back a step):

1. your Hetzner API token (hidden; or set `HCLOUD_TOKEN`),
2. the location (only those with a suitable server in stock right now),
3. the server type, with vCPU, RAM, disk and the monthly price (VAT
   included); ★ marks the recommendation, the 4 vCPU / 8 GB class (cx33,
   cax21), enough for the dashboard, its Postgres and a few apps,
4. a name (default `modula-main`), the dashboard domain, how DNS gets set
   (you add an A record, or Cloudflare does it with an API token), your
   email, and an SSH key (a local `~/.ssh/*.pub`, a key already in Hetzner,
   or none),
5. a summary with the monthly price to confirm.

Then it creates a Hetzner firewall (SSH, 80, 443 tcp+udp, 20000–29999, 7101)
and an Ubuntu 24.04 server that installs Modula on first boot, and shows
each step live as it happens, Hetzner's and the installer's own (the
installer reports them over plain HTTP on port 80 while it runs, so no SSH
is needed). If you add the DNS record yourself it shows the A record to add
right after the server gets its IP; the install carries on meanwhile.
Finally it asks for your admin name and password, creates the admin account
and logs this CLI in. You're ready for `modula init` and `modula up`.

```
  ✓ Firewall created                                       1s
  ✓ Server created: modula-main · cx33 · 203.0.113.10      3s

    Add this DNS record now (DNS only, not proxied): deploy.example.com  A  203.0.113.10
    The server installs meanwhile; the certificate waits for it.

  ✓ Booting Ubuntu                                         41s
  ✓ Checking the server                                    1s
  ✓ Updating packages, installing Docker, Postgres & Redis 1m 52s
  ✓ Setting up the firewall & hardening                    4s
  ✓ deploy.example.com points at 203.0.113.10
  ✓ Turning on security updates                            12s
  ✓ Installing Modula (latest)                            9s
  ✓ Setting up Postgres                                    3s
  ⠼ Starting services  14s · connection refused
  ✓ Services started                                       21s
  ✓ Dashboard is up (HTTPS)                                18s
  ✓ Your name · Admin
  ✓ Admin password · ********
  ✓ Admin account created (you@example.com)                1s
  ✓ API token created                                      0s

  ╭─ Your Modula server is ready!  (6m 30s)
  │
  │  Dashboard     https://deploy.example.com
  │  Server        modula-main · cx33 · Falkenstein · 203.0.113.10
  │  Cost          €8.21 per month, billed by Hetzner
  │  Admin         you@example.com (this CLI is logged in)
  │  SSH           ssh root@203.0.113.10
  │
  │  Next: modula init <name> && modula up
  ╰─  Have fun shipping things!
```

(The `⠼` line is the live spinner; on a terminal it updates in place.)

What happens to your secrets: the admin password stays in memory and is
wiped when the command ends. The Hetzner and Cloudflare tokens never go to
disk, logs or the server's user data; once your admin account exists, the
CLI saves them on the new server as the workspace's Hetzner and Cloudflare
integrations (encrypted, Settings → Integrations), so the dashboard can add
servers and manage DNS without asking again. The server gets a one-time setup token
(generated on your machine; only its hash is stored on the server) that the
CLI uses minutes later to create your admin. The only thing written to disk
is the new instance's API token, saved as a login named after the domain
(and made the default), exactly like `modula login`.

If anything fails after the server exists, you see the failing step and the
installer's hint, and you're asked whether to delete the server and its
firewall, so nothing keeps billing (`--on-failure` decides without asking).

To see what you could create, without creating anything:

```bash
HCLOUD_TOKEN=… modula server options                 # every location
HCLOUD_TOKEN=… modula server options --location fsn1 --json
```

## Log in to an existing server

```bash
modula login                       # asks for the server and a name for the login
modula login deploy.example.com    # or name the server right away
```

Your browser opens the dashboard's approval page; sign in if needed, check
that it shows the same code as your terminal, and approve. The CLI gets its
own API token (`CLI on <your machine>`, revocable under Account → Security).
Over SSH, `--browserless` prints the URL to open elsewhere.

Logins are saved by name, so several instances work side by side:
`modula login list`, `modula login use <name>` to switch the default,
`--login <name>` for one command. A directory you `link` keeps using the
login it was linked with.

Without a terminal (CI, agents): skip `login` and set `MODULA_URL` and
`MODULA_TOKEN` (a token from Account → Security → API tokens, `mdt_…`) for
every command, or save one with `modula login <url> --token …`.
`modula whoami` checks it. Instances in private mode (Cloudflare Access)
also need `MODULA_CF_ACCESS_CLIENT_ID` / `MODULA_CF_ACCESS_CLIENT_SECRET`.

## Deploy an app

```bash
cd my-app
modula init my-app          # create a project and link this directory
modula up                   # upload, build and deploy this directory
modula domain               # a generated https URL
modula logs -f              # follow the logs
```

Databases: `modula add --database postgres`, then reference its variables
from your app with `modula variables set 'DATABASE_URL=${{postgres.DATABASE_URL}}'`.
From GitHub instead of local files: `modula add --repo owner/name`.
`modula status` shows every service's latest deployment.

## Non-interactive / agent usage

Rules that hold for every command:

- **Nothing waits for input without a terminal.** A missing answer exits 2
  with what to pass. `--json` never prompts either, even on a terminal.
- **`--json`** prints machine-readable results on stdout; hints and
  progress go to stderr.
- **`--yes`** confirms destructive or billable actions; without a terminal
  they require it.
- **Secrets never go in flags** (they'd be in shell history and process
  lists): use environment variables or stdin.

### Creating a server (the agent recipe)

```bash
export HCLOUD_TOKEN=…                    # Hetzner Cloud API token, Read & Write
export MODULA_ADMIN_PASSWORD=…           # at least 10 characters
# export CLOUDFLARE_API_TOKEN=…          # with --cloudflare-dns (Zone → DNS → Edit)

# 1. Pick a location and type (read-only).
modula server options --json | jq '.locations[] | {name, recommended}'

# 2. Create it. Every answer is a flag; --yes because it costs money.
modula server create --provider hetzner \
  --location fsn1 --type cx33 --name modula-main \
  --domain deploy.example.com --email you@example.com \
  --ssh-key-file ~/.ssh/id_ed25519.pub \
  --admin-name 'Ada Lovelace' \
  --cloudflare-dns \
  --yes --json
```

| Flag | Meaning |
|---|---|
| `--provider hetzner` | The only provider so far (default). |
| `--location`, `--type` | From `server options`. |
| `--name` | Server name, a DNS label (default `modula-main`). |
| `--domain`, `--email` | Dashboard hostname (or `auto`: no domain yet, the dashboard at `https://<ip>` with an IP certificate); email for TLS and the admin. |
| `--ssh-key-file PATH` / `--ssh-key-id ID` / `--no-ssh-key` | Exactly one is required without a terminal. A file's key is reused if Hetzner has it. |
| `--cloudflare-dns` [`--replace-dns`] | Create the A record with Cloudflare; replace existing A/AAAA/CNAME records for the domain (otherwise their presence is an error). |
| `--admin-name` | The admin's display name (default `Admin`). |
| `--on-failure delete\|keep\|ask` | After the server exists. Default without a terminal: `delete`. |
| `--dns-timeout`, `--install-timeout` | Minutes to wait (30 and 20). |
| `--token-stdin` | Read the Hetzner token from stdin's first line instead of `HCLOUD_TOKEN`. |

| Variable | Used by |
|---|---|
| `HCLOUD_TOKEN` | `server create`, `server options` |
| `CLOUDFLARE_API_TOKEN` | `server create --cloudflare-dns` |
| `MODULA_ADMIN_PASSWORD` | `server create` without a terminal |
| `MODULA_URL`, `MODULA_TOKEN` | every other command (instead of `login`) |
| `MODULA_LOGIN` | which saved login to use (like `--login`) |
| `MODULA_PROJECT`, `MODULA_ENVIRONMENT`, `MODULA_SERVICE` | defaults for `-p`, `-e`, `-s` |

Everything is checked before anything is created: all missing flags and
variables are reported together (exit 2), and so are an unknown location or
type, an SSH key id that doesn't exist, a Cloudflare zone the token can't
see or conflicting DNS records.

**No domain yet?** `--domain auto` serves the dashboard at `https://<ip>`
(a Let's Encrypt IP certificate); nothing to set up. Add your domain later in
the dashboard or with `modula instance domain set`. Until then apps are
reached through custom domains or a TCP port.

**Without `--cloudflare-dns`** you (or the agent) must add the A record once
the IP is known: watch for the `dns_waiting` event, which carries the
record, and add it within `--dns-timeout`. With `--cloudflare-dns` nothing
needs doing.

With `--json`, progress is one JSON object per line on stdout. Every step
is a `step` event when it starts (`"state":"running"`) and when it ends
(`done` with `elapsedSecs`, `skipped`, or `failed`); milestones carry data:

```json
{"event":"plan","provider":"hetzner","location":"fsn1","type":"cx33","name":"modula-main","domain":"deploy.example.com","monthlyCents":821,"monthly":"€8.21"}
{"event":"step","name":"Creating the firewall","state":"running"}
{"event":"step","name":"Creating the firewall","state":"done","elapsedSecs":1}
{"event":"firewall_created","id":1001,"name":"modula-modula-main","reused":false}
{"event":"step","name":"Spinning up the server","state":"running"}
{"event":"step","name":"Spinning up the server","state":"done","elapsedSecs":3}
{"event":"server_created","id":5000,"name":"modula-main","ip":"203.0.113.10"}
{"event":"dns_waiting","recordType":"A","name":"deploy.example.com","value":"203.0.113.10","current":[]}
{"event":"step","name":"Booting Ubuntu","state":"running"}
{"event":"server_running","id":5000,"elapsedSecs":20}
{"event":"step","name":"Booting Ubuntu","state":"done","elapsedSecs":41}
{"event":"step","name":"Updating packages, installing Docker, Postgres & Redis","state":"running"}
{"event":"dns_ready","name":"deploy.example.com","ip":"203.0.113.10","elapsedSecs":130}
…
{"event":"step","name":"Starting services","state":"done","elapsedSecs":21}
{"event":"setup_complete","email":"you@example.com"}
{"event":"logged_in","url":"https://deploy.example.com/","email":"you@example.com"}
{"event":"ready","url":"https://deploy.example.com","ip":"203.0.113.10","serverId":5000,"elapsedSecs":390}
{"event":"result","ok":true,"exitCode":0,"url":"https://deploy.example.com","server":{…},"loggedIn":true,"email":"you@example.com","config":"…"}
```

Other events: `ssh_key_uploaded`, `dns_record_created` (with
`--cloudflare-dns`, instead of `dns_waiting`), `install_failed` (the
installer's `step`, `message` and `hint`), `error` (`message`, `hint`) and
`cleanup` (what was deleted). The last line is always `result`; on failure
it has `"ok":false`, `exitCode`, `error`, `hint`, and `serverKept` with the
`server` if one still exists.

### Exit codes

| Code | `server create` | Every command |
|---|---|---|
| 0 | Ready, and this CLI is logged in. | Success. |
| 1 | Failed; nothing left running (or it was deleted). | An error. |
| 2 | Missing or invalid flags; nothing was created. | Usage error (also: a missing `--yes` without a terminal). |
| 3 | A server exists but isn't fully set up; the result's `hint` says what to do (e.g. finish setup at `/setup` with the setup token it prints). | — |

### Other commands without a terminal

- `login` needs the URL and `--token` (or `MODULA_URL`/`MODULA_TOKEN`);
  otherwise it prints the approval URL and waits up to 10 minutes for a
  person to approve it.
- Nothing is ever picked from a list: a missing or ambiguous project,
  service, workspace, deployment or login is an error naming the choices
  (the same holds with `--json`, `--yes` or `CI` set).
- Destructive commands (`down`, `domain delete`, `variables set --sealed`,
  `db query --write`, `config apply`, …) need `--yes`.
- `variables set --stdin` reads `KEY=VALUE` lines from stdin, keeping values
  out of argv.
- `connect` starts an interactive database client; use `db query` (or
  `connect --tunnel-only`) from scripts.
- `up --ci` streams build logs, never prompts, and exits non-zero unless
  the deployment goes live.

## Troubleshooting

**"Hetzner rejected the API token"** — copy it again; it must be from the
project you want the server in, with Read & Write.

**A location or type is missing** — only what's in stock right now is
offered (`modula server options`). Try another location, or the arm64
(`cax…`) types.

**Stuck at "Waiting for deploy.example.com → IP"** — the A record isn't
visible to public resolvers yet. Check it with `dig +short
deploy.example.com @1.1.1.1`. Behind Cloudflare it must be **DNS only**
(grey cloud), not proxied: Let's Encrypt and the node port need to reach the
server directly. An old record pointing elsewhere must go.

**A step failed** — the CLI shows the installer's own message and hint.
The whole output is in `/var/log/cloud-init-output.log` on the server (keep
the server with `--on-failure keep` to look).

**Stuck at "Booting Ubuntu"** — cloud-init hasn't started the installer
yet; give it a few minutes. If the steps never show up, port 80 may be
blocked; the CLI still notices when the dashboard answers.

**Stuck at "Starting services" or "Getting a TLS certificate"** — the installer logs to
`/var/log/cloud-init-output.log`: `ssh root@<ip> tail -f
/var/log/cloud-init-output.log`. "getting a certificate" means Let's Encrypt
can't reach the server on port 80 yet (DNS, or a firewall in front).

**Setup didn't finish** (exit 3) — the server works; open
`https://<domain>/setup` and use the setup token the CLI printed. If it's
lost: `ssh root@<ip>`, remove `MODULA_SETUP_TOKEN_HASH` from
`/etc/modula/server.env`, then `systemctl restart modula-server` and
`journalctl -u modula-server` shows a new token.

**Something failed and the server was kept** — delete it (and the
`modula-<name>` firewall) in the Hetzner console, or run the command again
with the same `--name` (the firewall is reused).

**"…isn't accepted: secrets in arguments…"** — pass secrets through
`HCLOUD_TOKEN`, `CLOUDFLARE_API_TOKEN`, `MODULA_ADMIN_PASSWORD` or
`--token-stdin`, never as flags.

**"the API token was rejected"** on other commands — the `mdt_` token was
revoked or expired: `modula login` again.
