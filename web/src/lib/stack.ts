/** Ids with a bundled icon in `assets/stack/`. */
export const STACK_ICON_IDS = [
  "nextjs", "nuxt", "remix", "svelte", "astro", "nestjs", "express", "fastify", "hono", "vue", "react", "vite",
  "django", "fastapi", "flask", "rails", "laravel", "rust", "go", "bun", "deno", "nodejs", "python", "ruby", "php",
  "java", "kotlin", "elixir", "swift", "html",
  "postgres", "mysql", "mariadb", "mongodb", "redis", "valkey", "clickhouse", "minio", "nginx", "caddy", "rabbitmq",
  "elasticsearch", "meilisearch", "docker", "github",
] as const;

/** Single-colour glyphs rendered with `currentColor` (via CSS mask) so they follow the theme. */
export const MONO_STACK_ICONS: ReadonlySet<string> = new Set([
  "nextjs", "remix", "express", "fastify", "django", "github", "rust", "flask", "deno",
]);

const KNOWN: ReadonlySet<string> = new Set(STACK_ICON_IDS);

/** Substring rules on the bare image name (registry, namespace and tag stripped). Order matters. */
const IMAGE_RULES: [match: string, id: string][] = [
  ["postgres", "postgres"],
  ["pgvector", "postgres"],
  ["mariadb", "mariadb"],
  ["mysql", "mysql"],
  ["mongo", "mongodb"],
  ["valkey", "valkey"],
  ["redis", "redis"],
  ["clickhouse", "clickhouse"],
  ["minio", "minio"],
  ["nginx", "nginx"],
  ["caddy", "caddy"],
  ["rabbitmq", "rabbitmq"],
  ["elasticsearch", "elasticsearch"],
  ["meili", "meilisearch"],
];
/** Exact names, for short words that would false-positive as substrings. */
const IMAGE_EXACT: Record<string, string> = {
  node: "nodejs", bun: "bun", deno: "deno", golang: "go", python: "python", ruby: "ruby", php: "php",
};

/** `ghcr.io/x/clickhouse-server:24@sha256:..` -> `clickhouse-server` */
export function imageBaseName(image: string): string {
  const noDigest = image.trim().toLowerCase().split("@")[0] ?? "";
  const last = noDigest.split("/").pop() ?? "";
  return last.split(":")[0] ?? "";
}

/**
 * Pick the icon id for a service. Precedence: explicit `stack` -> known image -> GitHub (repo) ->
 * Docker (unknown image) -> "generic".
 */
export function resolveStackIcon({ stack, image, repo }: { stack?: string | null; image?: string | null; repo?: string | null }): string {
  const s = stack?.trim().toLowerCase();
  if (s && KNOWN.has(s)) return s;
  // Buckets (self-hosted or R2) share a glyph rather than their server's logo.
  if (s === "bucket") return "bucket";
  if (image) {
    const base = imageBaseName(image);
    const exact = IMAGE_EXACT[base];
    if (exact) return exact;
    for (const [m, id] of IMAGE_RULES) if (base.includes(m)) return id;
  }
  if (repo) return "github";
  if (image) return "docker";
  return "generic";
}
