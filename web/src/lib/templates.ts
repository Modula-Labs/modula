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

/** What's typed into a service name field, made one as you type: lowercase,
 *  each run of other characters one `-`, no leading digits or `-`, at most 63.
 *  A trailing `-` stays (so `postgres-` can go on to `postgres-api`);
 *  {@link serviceName} drops it. "Postgres API" -> `postgres-api`. */
export function slugifyServiceName(raw: string): string {
  return raw
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^[^a-z]+/, "")
    .slice(0, 63);
}

/** The service name a field's text stands for (empty when there's none). */
export const serviceName = (raw: string) => slugifyServiceName(raw).replace(/-+$/, "");

/** A DNS label starting with a letter (the server's rule); `shared` is the
 *  shared-variable prefix. */
export const validServiceName = (n: string) => /^[a-z]([a-z0-9-]{0,61}[a-z0-9])?$/.test(n) && n !== "shared";

/** Template id -> bundled brand icon id (see `stack.ts`). */
export function templateStack(template: string | null | undefined): string | null {
  if (!template) return null;
  if (template === "r2") return "bucket";
  return template === "mongo" ? "mongodb" : template;
}

export const isTemplateId = (t: string | null | undefined): t is TemplateId =>
  t === "postgres" || t === "mysql" || t === "redis" || t === "mongo" || t === "bucket";

/** Database templates (connection strings, backups), as opposed to buckets. */
export const isDatabaseTemplate = (t: string | null | undefined) =>
  t === "postgres" || t === "mysql" || t === "redis" || t === "mongo";

/** A bucket service: self-hosted (`bucket`) or Cloudflare R2 (`r2`). */
export const isBucketTemplate = (t: string | null | undefined) => t === "bucket" || t === "r2";

/** The variables every bucket service has, as the Connect view lists them. */
export const BUCKET_VARIABLES: { variable: string; label: string }[] = [
  { variable: "S3_ENDPOINT", label: "Endpoint" },
  { variable: "S3_BUCKET", label: "Bucket" },
  { variable: "S3_REGION", label: "Region" },
  { variable: "AWS_ACCESS_KEY_ID", label: "Access key ID" },
  { variable: "AWS_SECRET_ACCESS_KEY", label: "Secret access key" },
];

/** `media` in `production` → `media-production`, made a valid R2 bucket name. */
export function suggestBucketName(service: string, environment: string | undefined): string {
  const name = [service, environment]
    .filter(Boolean)
    .join("-")
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-+/, "")
    .slice(0, 63)
    .replace(/-+$/, "");
  return name.length >= 3 ? name : "bucket";
}

/** R2's naming rules: 3–63 lowercase letters, digits and hyphens, no hyphen at either end. */
export const validBucketName = (n: string) => /^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$/.test(n);

/** Where each database keeps its data; the suggested volume mount path. */
export function defaultMountPath(template: string | null | undefined): string {
  switch (template) {
    case "postgres":
      return "/var/lib/postgresql/data";
    case "mysql":
      return "/var/lib/mysql";
    case "mongo":
      return "/data/db";
    case "bucket":
      return "/data/bucket";
    default:
      return "/data";
  }
}
