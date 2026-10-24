// Static-site hosting on Cloudflare (mirrors the server's rules for `ServiceConfig.hosting`).

/** Variable-name prefixes that frameworks inline into the client bundle; they go to the build only. */
export const BUILD_ONLY_PREFIXES = ["VITE_", "NEXT_PUBLIC_", "PUBLIC_", "REACT_APP_", "NUXT_PUBLIC_", "GATSBY_", "EXPO_PUBLIC_"] as const;

/** Whether a variable is used at build time only (never set on the Worker). */
export function isBuildOnlyVariable(name: string): boolean {
  return BUILD_ONLY_PREFIXES.some((p) => name.startsWith(p));
}

/** The Worker name used when none is set: `<service>-<environment>`, lowercased, runs of non-alphanumerics become one dash. */
export function defaultScriptName(service: string, environment: string): string {
  return `${service}-${environment}`
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/** An error message for a Worker name, or null when it's valid (empty is valid: the default applies). */
export function validateScriptName(name: string): string | null {
  const n = name.trim();
  if (!n) return null;
  if (n.length > 63) return "At most 63 characters.";
  if (!/^[a-z0-9-]+$/.test(n)) return "Use lowercase letters, digits and dashes.";
  if (n.startsWith("-") || n.endsWith("-")) return "Can't start or end with a dash.";
  return null;
}

/** An error message for the output directory, or null when it's valid. */
export function validateOutputDir(dir: string): string | null {
  const d = dir.trim();
  if (!d) return "Required.";
  if (d.startsWith("/")) return "Use a path relative to the root directory.";
  if (d.split("/").includes("..")) return "Can't leave the root directory.";
  return null;
}

/** Where a Worker is served by default: `<name>.<your-subdomain>.workers.dev`. */
export function workersDevHost(scriptName: string): string {
  return `${scriptName || "<name>"}.<your-subdomain>.workers.dev`;
}

/** The first problem with the Cloudflare hosting fields, or null (also null when Cloudflare isn't selected). */
export function hostingError(f: { cloudflare: boolean; outputDir: string; scriptName: string }): string | null {
  if (!f.cloudflare) return null;
  const dir = validateOutputDir(f.outputDir);
  if (dir) return `Output directory: ${dir}`;
  const name = validateScriptName(f.scriptName);
  return name ? `Worker name: ${name}` : null;
}
