# CLI: `modula`

`modula` drives Modula Deploy from a terminal, a CI job or an AI agent:
projects, services, variables, deploys, logs, domains, volumes, TCP proxies
and databases. It mirrors the Railway CLI where the platforms line up.

```
Setup        login  logins  logout  whoami  list  init  link  unlink  service  open
Status       status  deployment list  logs
Services     add  variables  environment  domain  volume  tcp-proxy
Deploying    up  deploy  redeploy  restart  down  rollback
Databases    db query  db tables  connect
Shell        completion  docs  guide
Servers      server create  server options
```

`modula --help`, `modula <command> --help`, `--version`, and
`modula docs` (the full reference as Markdown, generated from the
binary itself) work everywhere.

## Install

Prebuilt CLI binaries aren't published yet. Build it from the repository
(Rust toolchain required):

```bash
cargo install --path crates/cli      # installs the `modula` binary
```

Shell completion:

```bash
modula completion zsh > "${fpath[1]}/_modula"   # bash, zsh, fish, elvish, powershell
```

## Authentication

Log in once per instance, in your browser:

```bash
modula login
# ◆ Modula server (domain or IP): deploy.example.com
# ◆ Save this login as: deploy.example.com
#   Your code: BCDF-GHJK  (check that the browser shows the same)
#   Opening https://deploy.example.com/cli/login?code=BCDF-GHJK in your browser…
#   Waiting for approval…
```

The dashboard page shows the same code and the machine's name; sign in if
needed, compare the code and click **Approve** (it asks for your password or
2FA code if you haven't re-authenticated in the last 15 minutes). The CLI then
gets an ordinary API token named `CLI on <hostname>`, listed and revocable
under **Account → Security → API tokens**. Codes expire after 10 minutes and
work once. `modula login deploy.example.com` skips the first question (a bare
host or IP gets `https://`); `--browserless` prints the URL instead of opening
a browser (over SSH, say), and the URL is printed whenever no terminal is
attached.

**CI and agents** don't use the browser: set environment variables and skip
`login` entirely, with a token created under **Account → Security**
([security → API tokens](security.md#api-tokens)):

```bash
export MODULA_URL=https://deploy.example.com
export MODULA_TOKEN=mdt_...
modula whoami
```

or save one non-interactively: `modula login https://deploy.example.com
--token "$MODULA_TOKEN" --name ci`.

| Variable | Effect |
|---|---|
| `MODULA_URL` | Instance URL. Overrides the saved one. Also the default for `login`'s URL. |
| `MODULA_TOKEN` | API token. Overrides the saved one. Also the default for `login --token`. |
| `MODULA_LOGIN` | Saved login to use, like `--login`. |
| `MODULA_PROJECT`, `MODULA_ENVIRONMENT`, `MODULA_SERVICE` | Defaults for `-p`, `-e`, `-s`. |
| `NO_COLOR` | No colors (they're also off when stdout isn't a terminal). |
| `XDG_CONFIG_HOME` | Where the config lives (default `~/.config`). |

**Several instances at once.** Each login is saved under a local name of your
choosing (`--name`, else asked for, defaulting to the host:
`deploy.example.com`, `localhost-8080`), in
`$XDG_CONFIG_HOME/modula/config.toml` (or `~/.config/modula/config.toml`),
readable only by you (mode 0600):

```toml
current = "work"

[logins.work]
url = "https://deploy.example.com/"
token = "mdt_…"

[logins.home]
url = "https://10.0.0.5/"
token = "mdt_…"
```

The newest login becomes the default (`current`). `modula login list` (or
`modula logins`) shows them, `modula login use <name>` switches the default,
and `--login <name>` (or `MODULA_LOGIN`) picks one for a single command.
Logging in again under an existing name replaces it (asking first when it
was for another instance).

**A linked directory remembers its login.** `link` (and every command that
rewrites `.modula/project.toml`) records the login in use as `login = "…"`;
commands run there use it even after you switch the default. If that login
is gone, they fail with a message naming it; log in again under that name,
pass `--login`, or link again.

**A token for one directory.** `login --local` (or `link --token`) saves the
login in `.modula/token` in the linked directory (else the current one)
instead: one `url` and `token`, mode 0600, found from any subdirectory, and
kept out of git (see [the project link](#the-project-link)). Useful for a
token scoped to one workspace, or one per repository.

The token used is the first of:

1. `MODULA_TOKEN` (with `MODULA_URL`, or the URL of the login below),
2. the login named by `--login` / `MODULA_LOGIN`,
3. the nearest `.modula/token`,
4. the login the directory was linked with,
5. the default login.

A `.modula/token` is only used when it's a regular file owned by you, readable
only by you (no group or other permission bits) and not tracked by git;
otherwise it's ignored with a warning, so a cloned repository can't point your
CLI at someone else's instance. When it points at a different instance than
your default login, the CLI says `using instance <url> from <path>` on stderr.

The CLI refuses to send a token over plain `http://` to anything but
`localhost`; pass `--allow-insecure-http` or set
`MODULA_ALLOW_INSECURE_HTTP=1` to override (it warns).

**What a token can't do.** Tokens never have sudo (a recent
re-authentication), so these always fail with "this needs a recently
re-authenticated dashboard session" and must be done in the dashboard:
revealing variable values (`config plan`/`pull` do read non-sealed values
through an audited Member route; see [config as code](config-as-code.md)),
deleting volumes (`volume delete`), deleting services, environments or
projects, and rotating database passwords.

## Global flags

Every command accepts these, before or after the command name:

| Flag | Description |
|---|---|
| `--json` | Machine-readable output: the result as JSON on stdout, nothing else. Hints and progress go to stderr. `logs --json` prints one JSON object per line. |
| `-p, --project <PROJECT>` | Project id or name. Default: the linked one. |
| `-e, --environment <ENV>` | Environment name or id. Default: the linked one, else `production`. |
| `-s, --service <SERVICE>` | Service name or id. Default: the linked one, else the project's only service. |
| `-y, --yes` | Skip confirmations. Destructive commands ask first; when stdin isn't a terminal they refuse (exit code 2) unless `--yes` is given. |
| `--login <NAME>` | Saved login to use (`MODULA_LOGIN`). Default: the directory's, else the current one. See [authentication](#authentication). |

Commands that confirm: `down`, `rollback`, `variables delete`,
`variables set --sealed`, `domain delete`, `volume delete`,
`tcp-proxy disable`, `db query --write`, `config apply`.

**Pickers.** At a terminal, a missing or ambiguous choice is asked for: when
there are several workspaces, projects or services and none was given (or
two projects share a name), `link` with several environments, `connect` with
several databases, `rollback` without a deployment, `members remove` without
an email, `login use` without a name. Arrow keys move, typing filters, Esc
cancels. Without a terminal (stdin or stdout redirected), with `--yes`,
`--json`, or `CI` set, nothing is asked: the command fails with the choices
listed, as before.

## Exit codes

`0` on success, `1` on error (the message is printed as `error: …` on
stderr), `2` for usage errors (unknown flags, bad `KEY=VALUE`, a missing
`--yes`). `up`, `deploy --wait`, `redeploy --wait` and `rollback --wait` exit
`1` when a deployment fails or crashes. `connect` exits with the database
client's exit code.

## The project link

`link`, `init`, `service` and `environment` write `.modula/project.toml`:

```toml
# Linked by modula; safe to commit, but usually gitignored.
project = "k3x9…"
project_name = "shop"
environment = "production"
service = "a81c…"
service_name = "api"
login = "work"
```

Commands look for the nearest `.modula/project.toml` in the current directory
or any parent. `-p/-e/-s` override it for one command; a `-p` naming another
project ignores the linked environment and service.
`login` is the [saved login](#authentication) it was linked with.

**`.modula/` stays out of git.** It holds the link, an optional local token
(`.modula/token`) and [config-as-code](config-as-code.md) secrets files
(`.modula/secrets.<env>.env`). Whenever the CLI writes into `.modula/`, it
makes sure the `.gitignore` at the repository root ignores it: it appends an
annotated `.modula/` block (creating `.gitignore` if the directory is in a git
repository and has none) and prints what it changed on stderr. A
`.gitignore` that already ignores `.modula/`, or names both `.modula/token`
and `.modula/secrets.*` (to commit `project.toml` on purpose), is left alone.
Outside a git repository only an existing `.gitignore` is updated. Commit
`modula.toml` at the repository root; it lives outside `.modula/`.

## Setup

### `login [URL] [--name <NAME>] [--browserless] [--local]`

Logs in through the browser (see [authentication](#authentication)): asks
for the instance when `URL` is omitted (suggesting saved ones; a bare host or
IP gets `https://`) and checks it answers, asks for the local name (default:
the host), then waits for you to approve the code in the dashboard. Saves the
login as the default and prints `Logged in to <url> as <email> (saved as
<name>, now the default)`. `--browserless` prints the approval URL without
opening a browser. `--local` saves it in `.modula/token` for this directory
only. For CI, `--token <TOKEN>` (or `MODULA_TOKEN`) uses an API token instead
of the browser; without a terminal, `URL` is required.

### `login list` (also `logins`)

The saved logins: name and URL; `*` marks the default. Notes a `.modula/token`
in effect here.

### `login use [NAME]`

Makes a saved login the default (picked from a list at a terminal when
`NAME` is omitted).

### `logout [NAME] [--all]`

Forgets a saved login: `NAME`, every one with `--all`, or by default this
directory's `.modula/token` if there is one, else the login in use. When the
default goes and one login is left, that one becomes the default. **The token
stays valid** until you delete it (Account → Security, or
`DELETE /api/account/tokens/{token_id}`).

### `whoami`

Your name, email, instance and which login is in use, then each workspace with
your role.

### `list` (alias `ls`)

Every project you can see: workspace, name, id; the linked one is marked.

### `init <NAME> [-w, --workspace <WORKSPACE>]`

Creates a project (with a `production` environment) and links the current
directory to it. `--workspace` takes an id or name; default: your only one.

### `link [--token <TOKEN>]`

Links the current directory to `-p` (default: your only project), `-e`
(default `production`) and, with `-s`, a default service. `--token` also
checks that token and saves it in `.modula/token` (like `login --local`;
the URL comes from `MODULA_URL` or the saved login). Keeps `.modula/` out of
git (see [the project link](#the-project-link)).

### `unlink`

Removes the nearest `.modula/project.toml` (and `.modula/` if it's now empty).

### `service [NAME]`

With a name: makes it the directory's default service. Without: lists the
project's services and marks the selected one.

### `open`

Prints the dashboard URL of the project (with the environment and service
selected) and opens it in a browser when run in a terminal.

## Status

### `status`

The project, environment, default service, the number of pending changes,
and every service in the environment: the **status of its newest
deployment** (`queued`, `building`, `deploying`, `active`, `failed`,
`crashed`, `removed`, `cancelled`, or `not deployed`), its first public URL,
its source (image, repository or database template) and the deployment id.

```bash
modula status --json | jq '.services[] | {name, status, url}'
```

### `deployment list [-l, --limit <N>]` (alias `deployments ls`)

The service's deployments, newest first: id, status, created (UTC), source
and commit. Default limit 20; the API keeps the newest 50.

### `logs [-d, --deployment <ID>] [-b, --build] [-f, --follow] [-n, --lines <N>]`

A deployment's runtime logs (`--build`: build logs). Default: the service's
newest deployment, the last 200 lines (`--lines`, at most 5000), then exit.
`--follow` keeps streaming until Ctrl-C or the deployment ends. Secret values
are masked by the server. Logs come from the server running the deployment,
so they fail with "the server running this deployment is offline" when it is.

## Services

### `add`

```
modula add (--database <ENGINE> | --image <REF> | --repo <OWNER/NAME> [--branch <BRANCH>] | --template <APP>
            | --function <FILE> [--cloudflare] [--compatibility-date <DATE>])
                  [--name <NAME>] [--port <PORT>] [-v, --variables KEY=VALUE]…
```

| Flag | Description |
|---|---|
| `-d, --database` | `postgres`, `mysql`, `redis` or `mongo`: the template, with a volume and generated credentials. |
| `-i, --image` | A prebuilt image, e.g. `ghcr.io/acme/api:1.2`. |
| `-r, --repo` | A GitHub repository, built on every deploy (needs the GitHub App installed for the workspace). |
| `-b, --branch` | With `--repo`; default `main`. |
| `-f, --function` | A JavaScript function from this file (a module worker, up to 1 MB), run in workerd on your servers; see [functions](projects-and-services.md#functions). The name defaults to the file's (`index.js` → `function`). |
| `--cloudflare` | With `--function`: run it on Cloudflare Workers instead. |
| `--compatibility-date` | With `--function`: the Workers runtime version it is written for; default `2026-10-01`. |

| `-t, --template` | A one-click app with the databases it needs: `n8n`, `plausible`, `umami`, `metabase`, `uptime-kuma` or `ghost` (`--template list` lists them). Creates every service, volume, generated secret and reference, generates a domain for the app and deploys it all in the selected environment. `--name` becomes a prefix (`blog` → `blog-ghost`, `blog-mysql`); taken names get `-2`. |
| `-n, --name` | Service name; default from the engine, image or repository name (lowercase letters, digits, `-`). |
| `--port` | The port the app listens on (image and repo); default 8080. |
| `-v, --variables` | Variables to set in the selected environment, repeatable. |

The service exists in every environment. Nothing is deployed yet (except
with `--template`). When the
directory is linked to the project, a new image or repo service becomes its
default service.

### `variables [--shared] [--kv]` (aliases `variable`, `vars`)

Lists variable names with `sealed`, references and last update. **Values are
write-only over the API**: listings show `********`, and `--kv` prints
`NAME=********` lines. Reveal values in the dashboard (`config pull` writes
non-sealed ones into `modula.toml`, except credential-like names). `--shared` acts on the
environment's shared variables (`${{shared.NAME}}`) instead of the service's.

### `variables set KEY=VALUE… [--stdin] [--sealed] [--deploy] [--shared]`

Sets variables; each argument is split at the first `=`, so values may
contain `=` and references like `${{postgres.DATABASE_URL}}` (quote them in
the shell). `--stdin` reads dotenv-style lines (`KEY=VALUE`, `export KEY=…`,
`#` comments, quoted values), which keeps secrets out of shell history.
`--sealed` stores them sealed: write-only forever, never copied to new
environments (asks first).

Changes **stay pending until deployed** (`status` counts them). `--deploy`
deploys right away: the service, or with `--shared` every service with
pending changes; with `--json` it then prints only the deploy result.
Railway's `--skip-deploys` is accepted and changes nothing,
because nothing deploys on its own here.

### `variables delete KEY… [--deploy] [--shared]` (alias `rm`)

Deletes variables by name (asks first). Pending until deployed, like `set`.

### `environment [NAME]` (alias `env`)

With a name: links the directory to that environment (keeping its service).
Without: lists environments and marks the selected and the default one. The
link stores the environment's id, so renaming it doesn't break the link.

### `environment new <NAME> [--duplicate <ENV>] [--branch <SERVICE=BRANCH>]…`

Creates an environment and links it. `--duplicate` copies that environment's
service settings and all its variables (sealed ones stay sealed); without it
the environment is empty: every service with the default environment's
settings and no variables. Databases always get an empty volume and fresh
credentials; domains and TCP proxies are not copied. `--branch` picks the
branch a repo service deploys from there (default: the same branch).
Nothing is deployed yet: run `deploy` there.

### `environment rename <ENV> <NAME>`

Renames an environment.

### `environment delete <ENV>`

Deletes an environment (asks first; never the default one). It needs a
re-authenticated dashboard session, so with an API token it fails and prints
the Project Settings → Environments link to do it there.

### `domain [--port <PORT>]`

Generates a domain under the instance's apps domain
(`<service>[-<env>]-xxxx.<apps domain>`) and prints its URL; `--port` routes
it to that port instead of the service's. Needs `MODULA_APPS_DOMAIN` on the
instance. At most 5 generated domains per service.

### `domain add <HOST> [--port <PORT>]`

Adds a custom domain and prints the DNS records to create as a
`TYPE NAME VALUE` table (a CNAME or A records for routing and
`TXT _modula-challenge.<host>` for ownership). With a DNS provider connected to
the workspace they are created for you. Then run `domain check`.

### `domain list` (alias `ls`), `domain check <HOST>`, `domain delete <HOST>` (alias `rm`)

`list`: hostname, kind, status (`pending`, `verified`, `failed`), port, last
error. `check`: looks up the DNS records now (at most every 5 seconds per
domain) and prints the status. `delete` asks first.

### `volume`, `volume add <PATH>`, `volume delete`

Without a subcommand: where the service's volume is mounted. `add` attaches a
volume at an absolute path (one per service); it is attached on the next
deploy. `delete` removes the volume and its data. It asks first, and it needs
a re-authenticated dashboard session, so with an API token it always fails:
delete volumes in the dashboard.

### `tcp-proxy`, `tcp-proxy enable [--port <PORT>]`, `tcp-proxy disable`

Without a subcommand: the service's TCP proxy (`host:public-port → port`).
`enable` exposes the service's port (or `--port`) on a public port from
20000–29999 of the server running it. **It is reachable from the internet**;
prefer `connect`, which needs no public port. `disable` removes it (asks
first).

## Deploying

`up` uploads the files in the current directory and deploys them, like
Railway's `railway up`: no GitHub needed. The other commands deploy the
service's configured source: its image, or its repository branch's latest
commit (push first).

### `up [--path DIR] [-d, --detach] [-m, --message TEXT] [--ci]`

Packs the directory (default: the current one) into a gzipped tarball,
uploads it to the selected service (`-s`, `-e`, or the link) and deploys it:
the service builds it exactly like a repository checkout, with its builder
(Railpack or Dockerfile), root directory, build and start commands, and the
config file in the files (`railway.toml`, `railway.json`, `modula.toml`). The
service's configured source isn't changed: the deployment shows as
**Uploaded via CLI**, and the next `deploy` deploys the configured source
again.

What is uploaded:

- Everything in the directory except what `.gitignore` (in any directory,
  plus `.git/info/exclude` and your global Git excludes) or `.modulaignore`
  (same syntax, for files Git keeps but the build doesn't need) leave out.
  This works without a Git repository too.
- Never `.git/`, `.modula/`, or the CLI's own config file. Symlinks pointing
  outside the directory are skipped.
- Up to 200 MB compressed by default (the server's `MODULA_UPLOAD_MAX_MB`) and
  200,000 files.

When the directory is a Git checkout, the deployment records the branch, the
commit and whether uncommitted changes were included (display only); the
deployment's message is `-m` or else the commit's subject.

Then `up` waits: it prints status changes, streams the build logs, and exits
`0` once the deployment is `active` (`1` if it fails or crashes). With
`--json` it skips the build logs and prints only the final deployment.
`--detach` returns right after starting it. `--ci` is for pipelines: never
prompts, streams the build logs, exits non-zero unless the deployment goes
live, and prints the last runtime logs of a crash.

```bash
modula up                         # the selected service, from here
modula up --path apps/web -s web -m "Hotfix"
MODULA_TOKEN=... modula up --ci -p shop -e staging -s api
```

Uploads are kept for 24 hours, and as long as one of the service's 5 newest
deployments (or a running one) uses them, so `redeploy` and `rollback` can
rebuild them; after that, run `up` again.

### `deploy [--all] [-w, --wait]`

Deploys pending changes. With a service selected (`-s` or the link) it
deploys that service's current config and variables, pending or not; without
one, or with `--all`, it deploys every service in the environment that has
pending changes ("Nothing pending" if none). Prints the service, deployment
id and any per-service error. `--wait` waits for each deployment to settle and
exits `1` if one fails.

### `redeploy [-w, --wait]`

Deploys the service's newest deployment again: the same image, the same
commit rebuilt, or the same upload rebuilt (while it is kept), with the current
variables.

### `restart`

Restarts the container of the newest running (`active` or `crashed`)
deployment in place: same deployment, same image.

### `down`

Stops and removes the service's newest live deployment (asks first). The
service stays; deploy again to bring it back.

### `rollback [DEPLOYMENT] [-w, --wait]`

Rebuilds and deploys an earlier deployment's Git commit or upload (asks
first). Only deployments built from a repository, or from an upload that is
still kept, can be rolled back to; pick one from `deployment list` (at a
terminal, omit it to pick from the service's deployments).

## Config as code

The whole project in `modula.toml` at the repository root: services,
settings, variables (secrets stay out of the file), domains, volumes, TCP
proxies, with per-environment overrides. Full reference:
[config as code](config-as-code.md).

### `config init [--force] [--file <PATH>]` (alias `config pull`)

Writes `modula.toml` from the project (`-p` or the linked one), every
environment, without secret values.

### `config plan [-e <ENV> | --all-envs] [--file <PATH>] [--secrets-file <PATH>] [--from-env] [--update-sealed]`

Prints what `apply` would change (secret values masked; `--json` for the
structured plan). Exits 1 when the plan has errors.

### `config apply [same flags] [--yes] [--deploy]`

Applies the plan after confirming: environments, services, settings,
volumes and TCP proxies, variables, domains. `--deploy` deploys the changed
services afterwards. Deleting services or volumes stays manual (dashboard).

### `config validate [--file <PATH>]`

Checks the file and every environment's overrides offline.

## Databases

### `db query <SQL> [--write]`

Runs SQL on a Postgres service through the API (no tunnel, no client). By
default it is **one read-only statement** (`SELECT`, `WITH`, `VALUES`,
`TABLE`, `EXPLAIN`, `SHOW`) in a `READ ONLY` transaction that is rolled back.
`--write` allows writes, DDL and several statements (asks first; needs the
**Member** role). Prints the last result set (at most 1000 rows) as a table
and a summary (`3 rows (SELECT, 4 ms)`); `--json` prints
`{columns, rows, rowCount, command, durationMs, truncated}`. 30-second
timeout; every query is audited.

The service defaults to `-s`, else the linked service if it is a database,
else the project's only database.

```bash
modula db query "select id, email from users order by id desc limit 5"
modula db query "update users set plan = 'pro' where id = 42" --write --yes
```

### `db tables`

Schema, name and estimated row count of every table.

### `connect [SERVICE] [--tunnel-only] [--show-password] [-P, --port <PORT>]`

| Argument / flag | Description |
|---|---|
| `SERVICE` | Service name or id. Default: `-s`, else the linked database, else the project's only database service. |
| `--tunnel-only` | Only keep a local port open (for GUI clients); don't start a client. |
| `--show-password` | With `--tunnel-only`: print the connection string with its password. |
| `-P, --port <PORT>` | Local port to listen on (127.0.0.1). Default: a free one. |

`connect` opens a private tunnel over HTTPS (a WebSocket to
`/api/service-instances/{id}/tunnel`) to the service's running container. No
public port is opened on the server. Then:

- For database templates it starts the native client against the local end:
  `psql` (Postgres), `mysql` (MySQL), `redis-cli` (Redis), `mongosh` (MongoDB).
  Passwords go through `PGPASSWORD`, `MYSQL_PWD` or `REDISCLI_AUTH` so they don't
  appear in the process list (`mongosh` only takes them as arguments).
  Ctrl-C goes to the client (it cancels the query in `psql`), and the tunnel
  closes when the client exits; the exit code is the client's.
- With `--tunnel-only`, or when no client or credentials are available, it
  prints `Tunnel to … open on 127.0.0.1:<port>` plus a connection string
  (password masked unless `--show-password`), and stays open until Ctrl-C.

Credentials come from `/api/service-instances/{id}/database/credentials`, which
currently serves Postgres services only; for MySQL, Redis and MongoDB `connect`
keeps the tunnel open and you connect with the service's credentials yourself
(copy them from the service's Variables tab).

```bash
modula connect                       # psql into the only database
modula connect postgres -e staging   # a specific service and environment
modula connect --tunnel-only -P 5433 # for TablePlus, DBeaver, pgAdmin…
```

Limits: a tunnel closes after 30 minutes without traffic; at most 16 open
tunnels per user and 64 per server. Opening a tunnel needs the **Member** role.

## Shell

### `completion <SHELL>`

Prints a completion script for `bash`, `zsh`, `fish`, `elvish` or
`powershell`.

### `docs`

Prints [the guide for people and agents](cli-for-humans-and-agents.md)
followed by every command and flag as Markdown, generated from the binary.

### `guide`

Prints just [the guide for people and agents](cli-for-humans-and-agents.md):
install, creating a server on Hetzner, logging in, deploying, non-interactive
use (flags, env vars, `--json`, exit codes) and troubleshooting.

## Servers

### `server create` (alias `server new`)

Creates a Modula server on Hetzner Cloud from your machine: a wizard, or
fully from flags (`--location --type --name --domain --email`, one of
`--ssh-key-file/--ssh-key-id/--no-ssh-key`, `--cloudflare-dns
[--replace-dns]`, `--admin-name`, `--on-failure`, `--yes --json`). Secrets
only from `HCLOUD_TOKEN` (or `--token-stdin`), `CLOUDFLARE_API_TOKEN` and
`MODULA_ADMIN_PASSWORD`. Shows each step live (the installer's too), finishes
setup and logs you in. Exit codes: 0 ready, 1 failed (nothing left running),
2 usage, 3 a server exists but isn't fully set up. Details:
[create a server on Hetzner](cli-for-humans-and-agents.md#create-a-server-on-hetzner).

### `server options [--location <LOCATION>]`

Locations with a suitable server type in stock, each type's vCPU, RAM,
disk, architecture and monthly price, and the recommendation. Read-only.

## A typical session

```bash
modula init shop
modula add --database postgres
modula add --repo acme/api --port 3000
modula variables set 'DATABASE_URL=${{postgres.DATABASE_URL}}'
modula up                      # uploads this directory to api and waits
modula deploy -s postgres
modula domain                  # https://api-x1y2.apps.example.com
modula status
modula logs -f
```

## Not in the CLI yet

Editing one service setting directly (start command, health check, builder,
resources: use [`config apply`](config-as-code.md), the dashboard or
`PATCH /api/service-instances/{id}`), deleting services, environments and
projects, backups, cron, metrics, `run`/`shell`/`ssh`, scaling, and server
management. Use the dashboard or the [HTTP API](api.md) with the same token.
