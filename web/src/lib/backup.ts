import type { BackupDestinationChoice, BackupDestinations, BackupProvider } from "./api";

export const PROVIDERS: { value: BackupProvider; label: string }[] = [
  { value: "s3", label: "Amazon S3" },
  { value: "r2", label: "Cloudflare R2" },
  { value: "b2", label: "Backblaze B2" },
  { value: "minio", label: "MinIO / S3-compatible" },
];

/** Placeholder endpoint for a provider; `<...>` parts are for the user to fill in. */
export function endpointHint(provider: BackupProvider, region = ""): string {
  const r = region.trim() || "<region>";
  switch (provider) {
    case "r2":
      return "https://<account>.r2.cloudflarestorage.com";
    case "b2":
      return `https://s3.${r}.backblazeb2.com`;
    case "s3":
      return `https://s3.${r}.amazonaws.com`;
    case "minio":
      return "https://minio.example.com";
  }
}

/** Region value the provider expects when the user has none. */
export const defaultRegion = (provider: BackupProvider) => (provider === "r2" ? "auto" : "");

export const hourLabel = (h: number) => `${String(h).padStart(2, "0")}:00 UTC`;

export interface DestinationOption {
  value: string;
  label: string;
  hint?: string;
}
export interface DestinationGroup {
  label: string;
  options: DestinationOption[];
}

/**
 * Select groups for the places a database can be backed up to. A bucket or
 * local target already created for a bucket service or server is listed under
 * that entry, not under the storage targets.
 */
export function destinationGroups(d: BackupDestinations): DestinationGroup[] {
  const claimed = new Set<string>();
  for (const b of d.buckets) if (b.targetId) claimed.add(b.targetId);
  for (const s of d.servers) if (s.targetId) claimed.add(s.targetId);
  const s3 = d.targets.filter((t) => t.kind === "s3" && !claimed.has(t.id));
  const groups: DestinationGroup[] = [];
  if (s3.length) {
    groups.push({ label: "Storage targets", options: s3.map((t) => ({ value: `target:${t.id}`, label: t.name })) });
  }
  if (d.buckets.length) {
    groups.push({
      label: "Bucket services",
      options: d.buckets.map((b) => ({
        value: `bucket:${b.instanceId}`,
        label: `${b.projectName} / ${b.serviceName} (${b.environmentName})`,
        hint: b.template === "r2" ? "Cloudflare R2" : "Self-hosted",
      })),
    });
  }
  if (d.servers.length) {
    groups.push({
      label: "This server's disk",
      options: d.servers.map((s) => ({ value: "local", label: `Disk of ${s.name}` })),
    });
  }
  return groups;
}

/** Select value for a saved schedule's target. */
export function destinationValue(targetId: string, d: BackupDestinations): string {
  const bucket = d.buckets.find((b) => b.targetId === targetId);
  if (bucket) return `bucket:${bucket.instanceId}`;
  if (d.servers.some((s) => s.targetId === targetId)) return "local";
  return `target:${targetId}`;
}

/** Request fields for a select value, or null when nothing is chosen. */
export function destinationChoice(value: string): BackupDestinationChoice | null {
  if (value === "local") return { localDisk: true };
  if (value.startsWith("bucket:")) return { bucketInstanceId: value.slice(7) };
  if (value.startsWith("target:")) return { targetId: value.slice(7) };
  return null;
}

/** The first option's value, used until the user picks one. */
export function defaultDestination(groups: DestinationGroup[]): string {
  return groups[0]?.options[0]?.value ?? "";
}
