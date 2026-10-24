# Projects and services

## The model

```
Workspace ─ Project ─ Environment ─ Service instance ─ Deployment
```

- A **workspace** holds projects, members (Admin, Member, Deployer, Viewer),
  backup targets and the GitHub installation.
- A **project** is one canvas. It starts with a `production` environment.
- A **service** exists in every environment of its project. Its settings,
  variables, domains, volume and TCP proxy are per environment: that pairing is
  a *service instance*.
- A **deployment** is an immutable snapshot of a service instance's settings and
  resolved variables, running on one server.

## The canvas

The project canvas shows one card per service with its status (`online`,
`deploying`, `crashed`, `offline`), its first public domain and its source.
When a service's variables reference another service
(`${{postgres.DATABASE_URL}}`), the canvas draws an edge to it.

Edits to settings and variables don't touch running deployments. They show up
as pending changes in the **Deploy changes** bar; see
[deployments](deployments.md#pending-changes).

## Create a service

**+ New** on the canvas offers:

| Source | What happens |
|---|---|
| **GitHub repo** | `owner/name`, branch, optional root directory. Built on the server for every deployment, with Railpack (no Dockerfile needed) or your Dockerfile. Pushes to the branch deploy automatically. Needs the instance's GitHub App installed for the workspace; see [integrations](integrations.md#github). |
| **Docker image** | Pulled and run as-is, e.g. `ghcr.io/acme/api:1.4` or `nginx@sha256:…`. |
| **Database** | A template: PostgreSQL, MySQL, Redis or MongoDB, with a volume and generated credentials. See [databases](databases.md). |
| **Function** | JavaScript that answers HTTP requests, written in the dashboard's editor (from a template) or bundled from a repository. See [functions](#functions). |

Service names use lowercase letters, digits and `-`, start with a letter and are
at most 63 characters. The name is also the private hostname
(`<name>.modula.internal`) and the prefix in references (`${{name.VAR}}`), so
pick it with care.

Over the API:

```bash
curl -fsS -X POST -H "Authorization: Bearer $MODULA_TOKEN" -H 'content-type: application/json' \
  "$MODULA_URL/api/projects/$PROJECT/services" -d '{
    "name": "api",
    "config": {
      "source": { "type": "repo", "repo": "acme/api", "branch": "main" },
      "builder": "dockerfile",
      "port": 3000,
      "healthcheckPath": "/health"
    }
  }'
```

### Functions

A function is a module worker, the format of Cloudflare Workers:

```js
export default {
  async fetch(request, env, ctx) {
    return new Response(`Hello, ${env.GREETING}!`);
  },
};
```

Its variables (references included) are on `env`, as text. It runs in one of
two places, switched in its settings:

- **Your servers**: in [workerd](https://github.com/cloudflare/workerd), the
  open-source Workers runtime, on the server it is placed on (each replica on
  its own server). Functions are always warm; a server runs all of its
  functions in one workerd process, each in its own isolate behind the edge,
  so domains, edge rules and HTTP request logs work as for any service.
  `console.log` output is the deployment's logs. workerd runs as its own
  unprivileged user, and functions reach the public internet only: not the
  server, its other services, the private network or the cloud's metadata
  service. A deploy, or any change to a server's functions, restarts that
  server's workerd: it gets up to 5 seconds to finish requests in flight, and
  new ones see a brief gap.
- **Cloudflare**: uploaded as a Worker with the workspace's Cloudflare
  connection ([integrations](integrations.md#cloudflare)), served on
  `<worker>.<subdomain>.workers.dev` and its custom domains, with every
  variable as a binding (sealed and credential-like ones as secrets). Its logs
  are in the Cloudflare dashboard; the deployment links to them.

Code written in the dashboard is plain JavaScript, up to 1 MB, and deploys
without a build. A function from a repository names its entry file (default
`index.ts`; TypeScript is fine): each deploy installs the repository's
dependencies with its lockfile's package manager (without install scripts)
and bundles the entry with esbuild into one module. The compatibility date is
the Workers runtime version the code is written for; on your servers it can be
at most the date of the workerd release the servers run (2026-10-14).

```json
{ "source": { "type": "function", "code": "export default { … }", "compatibilityDate": "2026-10-01" } }
{ "source": { "type": "function", "repo": "acme/fns", "branch": "main", "entry": "src/index.ts", "compatibilityDate": "2026-10-01" },
  "hosting": { "type": "cloudflare" } }
```

From the CLI: `modula add --function hello.js [--cloudflare]`.

## Builders


For repository sources:

- **Railpack** (`"builder": "railpack"`, the default) detects the language and
  builds without a Dockerfile. Its start command is baked into the image.
- **Dockerfile** (`"builder": "dockerfile"`) uses `Dockerfile` in the root
  directory, or `dockerfilePath`.

### Config as code

A `modula.toml`, `railway.toml` or `railway.json` at the build root (the root
directory, if set) overrides dashboard settings for that build. The first file
found in that order wins; files are not merged. Only these keys are read:

| Section | Key | Values |
|---|---|---|
| `build` | `builder` | `RAILPACK`, `NIXPACKS` (built with Railpack), `DOCKERFILE` |
| `build` | `buildCommand` | string |
| `build` | `dockerfilePath` | relative path |
| `deploy` | `startCommand` | string, run with `/bin/sh -c` in the container |
| `deploy` | `healthcheckPath` | path starting with `/` |
| `deploy` | `healthcheckTimeout` | 1–3600 seconds |
| `deploy` | `restartPolicyType` | `ON_FAILURE`, `ALWAYS`, `NEVER` |
| `deploy` | `restartPolicyMaxRetries` | 0–1000 (0 = no limit) |
| `deploy` | `preDeployCommand` | string or list of strings; a list runs as `a && b && …` in `/bin/sh -c` |

Unknown keys (including `watchPatterns` and `environments`) are ignored: set
watch paths in the dashboard. A `modula.toml` that describes the whole
project for `modula config` (it has `project` or `services` and no
`[build]`/`[deploy]`) is not read here, so a `railway.toml` next to it still
applies; see [config as code](config-as-code.md) to manage every service's
settings from one file. An invalid value is reported as a build-log
warning and the dashboard setting is used instead. The build log says which
file was used and what it set.

```toml
# modula.toml
[build]
builder = "DOCKERFILE"

[deploy]
healthcheckPath = "/health"
healthcheckTimeout = 60
preDeployCommand = "bun run migrate"
```

## Settings reference

Each service instance has one `config` object (`PATCH
/api/service-instances/{id}` with `{ "config": { … } }`; send the whole object).
Unknown fields are rejected.

| Field | Type | Default | Limits / notes |
|---|---|---|---|
| `source` | object | required | `{ "type": "image", "image": "…" }` or `{ "type": "repo", "repo": "owner/name", "branch": "main", "rootDir": "apps/api" }` |
| `source.image` | string | — | Docker reference `[registry[:port]/]path[:tag][@sha256:…]`, ≤512 chars, lowercase path |
| `source.repo` | string | — | `owner/name` |
| `source.branch` | string | — | Valid git branch name, ≤255 chars, no spaces |
| `source.rootDir` | string | repo root | Relative path inside the repo, no `..` |
| `startCommand` | string[] | image default | argv, never run through a shell; ≤64 args, each ≤4096 bytes. Ignored for Railpack builds (the image's command runs). |
| `port` | number | `8080` | 1–65535. Injected as `PORT`; the port domains and health checks use. |
| `healthcheckPath` | string | none | Starts with `/`, ≤512 chars, no spaces. |
| `healthcheckTimeoutSecs` | number | `300` | 1–3600 |
| `restartPolicy` | string | `on_failure` | `on_failure`, `always`, `never` |
| `restartPolicyMaxRetries` | number | unlimited | 1–1000; only with `on_failure` |
| `preDeployCommand` | string[] | none | argv run once from the new image before it starts (e.g. migrations); ≤64 args, each ≤4096 bytes |
| `drainingSeconds` | number | `0` (server default) | 0–300. Time between SIGTERM and SIGKILL when a container is retired. |
| `memoryMb` | number | `512` | 16–1048576. Unset means the 512 MB default, so one app can't take the whole server's memory. |
| `cpuMillis` | number | `1000` | 10–256000 (1000 = one CPU). Unset means one CPU. |
| `replicas` | number | `1` | Only `1` for now |
| `builder` | string | `railpack` | `railpack` or `dockerfile` (repo sources) |
| `dockerfilePath` | string | `Dockerfile` | Relative to the root directory |
| `buildCommand` | string | detected | Replaces the detected build command; ≤4096 bytes |
| `autoDeploy` | boolean | `true` | Deploy on every push to the branch |
| `watchPaths` | string[] | all paths | Gitignore-style globs; only pushes touching them deploy. ≤50 entries, each ≤256 chars |

Settings take effect on the service's next deployment.

## Delete a service

**Settings → Delete service** removes it from every environment, stops its
deployments and deletes its volumes' data. It needs the **Member** role and a
recent re-authentication, so it can't be done with an API token.
