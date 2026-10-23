import type { BackupProvider } from "./api";

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
