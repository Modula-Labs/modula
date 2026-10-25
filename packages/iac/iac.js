// modula/iac: helpers for .modula/modula.ts (Modula Deploy config as code).
//
// The CLI embeds this file and resolves `modula/iac` to it when it runs
// modula.ts, so nothing has to be installed. The helpers build the same
// structure as .modula/modula.toml (see guide/config-as-code.md); the CLI
// validates it.

const KIND = Symbol.for("modula.iac.kind");

/** Problems worth a warning (printed once by the CLI). */
const warnings = [];
globalThis.__modulaIacWarnings = warnings;

function warn(message) {
  if (!warnings.includes(message)) warnings.push(message);
}

/** "Anton API" -> "anton-api": service names are lowercase slugs. */
export function slug(name) {
  if (typeof name !== "string" || name.trim() === "") {
    throw new TypeError("a service name must be a non-empty string");
  }
  return name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/** The config function (or object); returned as is, for types. */
export function defineModula(config) {
  return config;
}

// ------------------------------------------------------------------ sources

/** A GitHub repository: "owner/name" (or its URL). */
export function github(repo, options = {}) {
  if (typeof repo !== "string") throw new TypeError("github(): repo must be a string");
  const name = repo
    .trim()
    .replace(/^https?:\/\/github\.com\//, "")
    .replace(/^git@github\.com:/, "")
    .replace(/\.git$/, "")
    .replace(/\/+$/, "");
  const source = { repo: name };
  if (options.branch != null) source.branch = options.branch;
  if (options.rootDir != null) source.rootDir = options.rootDir;
  return source;
}

/** A prebuilt image, e.g. "ghcr.io/acme/api:1.2". */
export function image(ref) {
  if (typeof ref !== "string") throw new TypeError("image(): the image must be a string");
  return { image: ref };
}

/** A directory deployed with `modula up`. */
export function local(path = ".") {
  return { local: path };
}

// ------------------------------------------------------------------ variables

/** Keep this variable as it is on the server (apply never deletes it). */
export function preserve() {
  return { [KIND]: "preserve" };
}

/** A secret: the value comes from .modula/secrets.<env>.env or --from-env. */
export function secret(options = {}) {
  return { [KIND]: "secret", sealed: Boolean(options.sealed) };
}

/** A reference to another service's variable: ${{service.VAR}}. */
export function ref(service, variable) {
  if (typeof variable !== "string") throw new TypeError("ref(): the variable must be a string");
  const scope = service === "shared" ? "shared" : slug(service);
  return "${{" + scope + "." + variable + "}}";
}

const RAILWAY_HINTS = {
  RAILWAY_DEPLOYMENT_DRAINING_SECONDS: "drainingSeconds",
  RAILWAY_HEALTHCHECK_TIMEOUT_SEC: "healthcheckTimeout",
  RAILWAY_DOCKERFILE_PATH: "dockerfilePath",
};

function variables(where, vars, preserved) {
  const out = {};
  for (const [name, value] of Object.entries(vars ?? {})) {
    if (value === undefined || value === null) continue;
    if (name.startsWith("RAILWAY_")) {
      const hint = RAILWAY_HINTS[name];
      warn(
        `${where}.variables.${name}: Modula ignores RAILWAY_ variables` +
          (hint ? ` (use the \`${hint}\` setting)` : ""),
      );
    }
    if (typeof value === "object" && value[KIND] === "preserve") {
      preserved.push(name);
    } else if (typeof value === "object" && value[KIND] === "secret") {
      out[name] = value.sealed ? { secret: true, sealed: true } : { secret: true };
    } else if (["string", "number", "boolean"].includes(typeof value)) {
      out[name] = value;
    } else {
      throw new TypeError(
        `${where}.variables.${name}: use a string, number, boolean, secret() or preserve()`,
      );
    }
  }
  return out;
}

// ------------------------------------------------------------------ services

// A command string without shell syntax is split on whitespace; anything
// else runs with `sh -c`.
const SHELL_SYNTAX = /["'`$|&;<>*?(){}\[\]\\~\n#]/;

export function command(value) {
  if (Array.isArray(value)) return value.map(String);
  if (typeof value !== "string") throw new TypeError("a command is a string or an array");
  const trimmed = value.trim();
  if (trimmed === "") return [];
  return SHELL_SYNTAX.test(trimmed) ? ["sh", "-c", trimmed] : trimmed.split(/\s+/);
}

// Friendlier and Railway names -> the model's keys.
const ALIASES = {
  start: "startCommand",
  preDeploy: "preDeployCommand",
  healthcheck: "healthcheckPath",
  healthcheckTimeout: "healthcheckTimeoutSecs",
  numReplicas: "replicas",
  restartPolicyType: "restartPolicy",
  watchPatterns: "watchPaths",
  cron: "cronSchedule",
  cronTimeout: "cronTimeoutSecs",
  memory: "memoryMb",
  cpu: "cpuMillis",
};

const COMMANDS = ["startCommand", "preDeployCommand"];

function settings(where, options, out) {
  for (const [key, value] of Object.entries(options)) {
    if (value === undefined || value === null) continue;
    if (key === "deploy" || key === "build") {
      if (typeof value !== "object") throw new TypeError(`${where}.${key} must be an object`);
      settings(where, value, out);
      continue;
    }
    if (key === "sleepApplication") {
      if (value) out.serverless = {};
      continue;
    }
    if (key === "overlapSeconds") {
      warn(`${where}.overlapSeconds: Modula has no overlap setting (ignored)`);
      continue;
    }
    const name = ALIASES[key] ?? key;
    if (name in out) throw new TypeError(`${where}: ${key} is set twice`);
    let v = value;
    if (COMMANDS.includes(name)) v = command(value);
    else if (name === "restartPolicy" || name === "builder") {
      v = String(value).toLowerCase().replace(/-/g, "_");
    }
    out[name] = v;
  }
}

function serviceSpec(where, options) {
  if (options === null || typeof options !== "object") {
    throw new TypeError(`${where}: options must be an object`);
  }
  const { variables: vars, preserve: keep, ...rest } = options;
  const spec = {};
  settings(where, rest, spec);
  const preserved = [...(keep ?? [])];
  const v = variables(where, vars, preserved);
  if (Object.keys(v).length) spec.variables = v;
  if (preserved.length) spec.preserve = preserved;
  return spec;
}

/** A service: a source (github(), image(), local()) and its settings. */
export function service(name, options = {}) {
  const id = slug(name);
  return { [KIND]: "service", name: id, spec: serviceSpec(`services.${id}`, options) };
}

/** A service from a template: postgres, mysql, redis, mongo, bucket or r2. */
export function database(template, name = template, options = {}) {
  if (typeof name === "object") {
    options = name;
    name = template;
  }
  const id = slug(name);
  const spec = serviceSpec(`services.${id}`, options);
  return { [KIND]: "service", name: id, spec: { template, ...spec } };
}

export const postgres = (name = "postgres", options = {}) => database("postgres", name, options);
export const mysql = (name = "mysql", options = {}) => database("mysql", name, options);
export const redis = (name = "redis", options = {}) => database("redis", name, options);
export const mongo = (name = "mongo", options = {}) => database("mongo", name, options);
export const bucket = (name = "bucket", options = {}) => database("bucket", name, options);
export const r2 = (name = "r2", options = {}) => database("r2", name, options);

// ------------------------------------------------------------------ project

/** The project: its services (`resources`) and shared variables. */
export function project(name, options = {}) {
  const services = {};
  for (const r of [...(options.resources ?? []), ...(options.services ?? [])]) {
    if (r === undefined || r === null || r === false) continue;
    if (r[KIND] !== "service") {
      throw new TypeError("project(): resources are service(), database(), postgres() …");
    }
    if (r.name in services) throw new TypeError(`project(): two services are named ${r.name}`);
    services[r.name] = r.spec;
  }
  const out = { services };
  if (name != null) out.project = name;
  if (options.partial) out.partial = true;
  const preserved = [...(options.preserve ?? [])];
  const vars = variables("shared", options.variables, preserved);
  const shared = {};
  if (Object.keys(vars).length) shared.variables = vars;
  if (preserved.length) shared.preserve = preserved;
  if (Object.keys(shared).length) out.shared = shared;
  return { [KIND]: "project", ...out };
}
