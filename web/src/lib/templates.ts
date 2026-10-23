import type { TemplateId } from "./api";

/** `postgres` -> `postgres`, or `postgres-2`, `postgres-3`... when taken. */
export function uniqueName(base: string, taken: Iterable<string>): string {
  const used = new Set([...taken].map((n) => n.toLowerCase()));
  if (!used.has(base.toLowerCase())) return base;
  for (let n = 2; ; n++) {
    const candidate = `${base}-${n}`;
    if (!used.has(candidate)) return candidate;
  }
}

/** Template id -> bundled brand icon id (see `stack.ts`). */
export function templateStack(template: string | null | undefined): string | null {
  if (!template) return null;
  return template === "mongo" ? "mongodb" : template;
}

export const isTemplateId = (t: string | null | undefined): t is TemplateId =>
  t === "postgres" || t === "mysql" || t === "redis" || t === "mongo";

/** Where each database keeps its data; the suggested volume mount path. */
export function defaultMountPath(template: string | null | undefined): string {
  switch (template) {
    case "postgres":
      return "/var/lib/postgresql/data";
    case "mysql":
      return "/var/lib/mysql";
    case "mongo":
      return "/data/db";
    default:
      return "/data";
  }
}
