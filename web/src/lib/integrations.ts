// Cloudflare integration API (mirrors crates/server/src/integrations/cloudflare/api.rs).
// The token is write-only: nothing here ever receives it back.

import { del, get, post, request, type BackupTarget } from "./api";

export interface CloudflarePermission {
  name: string;
  why: string;
  required: boolean;
}
export interface CloudflareStatus {
  connected: boolean;
  accountId: string | null;
  accountName: string | null;
  /** Last four characters, e.g. "…a1b2". */
  tokenHint: string | null;
  connectedBy: string | null;
  updatedAt: string | null;
  tokenTemplateUrl: string;
  permissions: CloudflarePermission[];
}
export interface CloudflareAccount {
  id: string;
  name: string;
}
export interface CloudflareZone {
  id: string;
  name: string;
  status: string;
  paused: boolean;
}
export interface CloudflarePreview {
  accounts: CloudflareAccount[];
  accountId: string | null;
  zones: CloudflareZone[];
}
export interface R2Info {
  accountId: string;
  endpoint: string;
  canCreateBuckets: boolean;
  apiTokensUrl: string;
}
export interface R2BucketLink {
  projectId: string;
  projectName: string;
  serviceId: string;
  serviceName: string;
  environmentId: string;
  environmentName: string;
}
export interface R2Bucket {
  name: string;
  creationDate: string | null;
  location: string | null;
  /** Services (per environment) whose credentials reach this bucket. */
  links: R2BucketLink[];
}
export interface R2Buckets {
  accountId: string;
  endpoint: string;
  buckets: R2Bucket[];
}
export interface DomainIngress {
  managed: boolean;
  zone: string | null;
  proxied: boolean;
  sslMode: string | null;
  sslWarning: string | null;
  note: string | null;
}

const base = (ws: string) => `/api/workspaces/${ws}/integrations/cloudflare`;

export const cloudflare = {
  status: (ws: string) => get<CloudflareStatus>(base(ws)),
  verify: (ws: string, token: string, accountId?: string) => post<CloudflarePreview>(`${base(ws)}/verify`, { token, accountId }),
  connect: (ws: string, token: string, accountId?: string) => request<CloudflareStatus>("PUT", base(ws), { token, accountId }),
  disconnect: (ws: string) => del<CloudflareStatus>(base(ws)),
  zones: (ws: string) => get<CloudflareZone[]>(`${base(ws)}/zones`),
  r2: (ws: string) => get<R2Info>(`${base(ws)}/r2`),
  r2Buckets: (ws: string) => get<R2Buckets>(`${base(ws)}/r2/buckets`),
  createR2Target: (
    ws: string,
    body: { name: string; bucket: string; prefix?: string; accessKeyId: string; secretAccessKey: string },
  ) => post<BackupTarget>(`${base(ws)}/r2-targets`, body),
  ingress: (domainId: string) => get<DomainIngress>(`/api/domains/${domainId}/ingress`),
  setIngress: (domainId: string, proxied: boolean) => request<DomainIngress>("PUT", `/api/domains/${domainId}/ingress`, { proxied }),
};

/** The zone a hostname lives in, among the given zone names (deepest match). */
export function zoneFor(hostname: string, zones: string[]): string | null {
  const host = hostname.replace(/^\*\./, "").replace(/\.$/, "").toLowerCase();
  let best: string | null = null;
  for (const z of zones) {
    const name = z.toLowerCase();
    if ((host === name || host.endsWith("." + name)) && (!best || name.length > best.length)) best = name;
  }
  return best;
}

/** Human label for a Cloudflare SSL/TLS mode. */
export function sslModeLabel(mode: string | null): string {
  switch (mode) {
    case "strict":
      return "Full (strict)";
    case "full":
      return "Full";
    case "flexible":
      return "Flexible";
    case "off":
      return "Off";
    default:
      return "Unknown";
  }
}
