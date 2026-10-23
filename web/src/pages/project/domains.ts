import type { Domain } from "@/lib/api";
import type { Tone } from "@/components/ui/misc";

/** Badge for a domain's verification state. */
export function domainStatus(d: Pick<Domain, "kind" | "status">): { label: string; tone: Tone } | null {
  if (d.kind === "generated") return null;
  switch (d.status) {
    case "verified":
      return { label: "Verified", tone: "success" };
    case "failed":
      return { label: "Failed", tone: "danger" };
    default:
      return { label: "Waiting for DNS", tone: "warn" };
  }
}

/** "" → null (the service's port); otherwise a port number or an error message. */
export function parsePort(raw: string): { port: number | null } | { error: string } {
  const s = raw.trim();
  if (s === "") return { port: null };
  if (!/^\d+$/.test(s)) return { error: "Enter a number" };
  const n = Number(s);
  if (n < 1 || n > 65535) return { error: "Ports are 1–65535" };
  return { port: n };
}

/** "" → null (no limit); otherwise requests per minute or an error message. */
export function parseRateLimit(raw: string): { limit: number | null } | { error: string } {
  const s = raw.trim();
  if (s === "") return { limit: null };
  if (!/^\d+$/.test(s)) return { error: "Enter a number" };
  const n = Number(s);
  if (n < 1 || n > 1_000_000) return { error: "1–1,000,000 per minute" };
  return { limit: n };
}
