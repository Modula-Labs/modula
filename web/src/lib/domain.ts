import { load, save } from "./storage";

export interface DnsRecord {
  type: "A";
  name: string;
  value: string;
  purpose: "dashboard" | "apps";
}

/** Lower-cases and strips scheme, path, port and trailing dot from what people paste ("https://Deploy.Example.com/"). */
export function normalizeHost(input: string): string {
  let s = input.trim().toLowerCase();
  s = s.replace(/^[a-z][a-z0-9+.-]*:\/\//, "");
  s = s.split(/[/?#]/)[0] ?? "";
  s = s.replace(/:\d+$/, "").replace(/\.$/, "");
  return s;
}

const LABEL = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

/** null when `host` (already normalized) is a plausible DNS name with at least two labels; otherwise a message. Empty is fine (clears). */
export function hostError(host: string): string | null {
  if (!host) return null;
  if (host.length > 253) return "That hostname is too long.";
  if (/^\d+(\.\d+){3}$/.test(host) || host.includes(":")) return "Use a domain name, not an IP address.";
  const labels = host.split(".");
  if (labels.length < 2) return "Enter a full domain such as deploy.example.com.";
  if (!labels.every((l) => LABEL.test(l))) return "Use letters, digits and hyphens only.";
  return null;
}

/** The A records to create for the given (normalized) domains. Empty without a server IP or domains. */
export function dnsRecords(dashboard: string, apps: string, serverIp: string | null): DnsRecord[] {
  if (!serverIp) return [];
  const out: DnsRecord[] = [];
  if (dashboard) out.push({ type: "A", name: dashboard, value: serverIp, purpose: "dashboard" });
  if (apps) out.push({ type: "A", name: `*.${apps}`, value: serverIp, purpose: "apps" });
  return out;
}

export function httpsUrl(host: string): string {
  return `https://${host}`;
}

const SKIP_KEY = "mc.domainSkipped";
const BANNER_KEY = "mc.domainBannerDismissed";

export const domainStepSkipped = () => load(SKIP_KEY) === "1";
export const skipDomainStep = () => save(SKIP_KEY, "1");
export const domainBannerDismissed = () => load(BANNER_KEY) === "1";
export const dismissDomainBanner = () => save(BANNER_KEY, "1");
