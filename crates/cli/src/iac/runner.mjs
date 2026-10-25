// Evaluates .modula/modula.ts for one environment and prints the result as
// JSON on stdout (see script.rs). Runs after a prelude that makes
// `modula/iac` resolve to the embedded module.
//
// In: MODULA_IAC_FILE (absolute path), MODULA_IAC_ENVIRONMENT (absent for
// the base), MODULA_IAC_PREVIEW ("1" for a pull request preview).
// Out: { spec, environments, warnings }.

const { pathToFileURL } = await import("node:url");

// The user's logging must not mix with the result.
const stdout = process.stdout;
console.log = console.error;
console.info = console.error;
console.debug = console.error;

// Exits once the message is written (pipes can be asynchronous).
async function fail(message) {
  await new Promise((done) => process.stderr.write(message + "\n", done));
  process.exit(1);
}

const file = process.env.MODULA_IAC_FILE;
const environment = process.env.MODULA_IAC_ENVIRONMENT?.toLowerCase() || undefined;
const ctx = Object.freeze({
  environment,
  isPreview: process.env.MODULA_IAC_PREVIEW === "1",
  isEnvironment: (...names) =>
    environment !== undefined && names.some((n) => String(n).toLowerCase() === environment),
});

let mod;
try {
  mod = await import(pathToFileURL(file).href);
} catch (e) {
  await fail(`cannot load ${file}:\n${e?.stack ?? e}`);
}

let config = mod.default;
if (config === undefined) {
  await fail(`${file} has no default export (export default defineModula(...))`);
}
try {
  if (typeof config === "function") config = await config(ctx);
} catch (e) {
  await fail(`${file} threw:\n${e?.stack ?? e}`);
}
if (config === null || typeof config !== "object" || Array.isArray(config)) {
  await fail(`${file}: the default export must be project(...) or a function returning it`);
}

const spec = { ...config };
if (mod.partial !== undefined && mod.partial !== false) spec.partial = true;

let environments = mod.environments ?? [];
if (!Array.isArray(environments) || environments.some((e) => typeof e !== "string")) {
  await fail(`${file}: \`export const environments\` must be an array of names`);
}

const out = {
  spec,
  environments,
  warnings: globalThis.__modulaIacWarnings ?? [],
};
stdout.write(JSON.stringify(out, (_, v) => (v === null ? undefined : v)) + "\n");
