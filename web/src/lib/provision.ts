// Cloud provider integrations (Hetzner, AWS EC2) and the servers created through
// them (crates/server/src/provision, crates/server/src/integrations/{hetzner,aws}).

import { del, get, patch, post, request, type LocationOption } from "./api";

const enc = encodeURIComponent;

export interface ProviderCaps {
  maxServers: number;
  /** USD cents per month. */
  maxMonthlyCents: number;
}
export interface ProviderUsage {
  servers: number;
  monthlyCents: number;
}
/** A provider the workspace has connected, with its limits and spend. */
export interface ConnectedProvider {
  kind: string;
  caps: ProviderCaps;
  usage: ProviderUsage;
}
export interface ProviderStatus {
  connected: boolean;
  connectedAt: string | null;
  connectedBy: string | null;
  caps: ProviderCaps;
  usage: ProviderUsage;
}
export interface HetznerStatus extends ProviderStatus {
  tokenUrl: string;
}
export interface AwsStatus extends ProviderStatus {
  /** Where to create an access key. */
  keyUrl: string;
  /** The minimal IAM policy for its user (a JSON document). */
  iamPolicy: Record<string, unknown>;
}
export interface ProviderLocation {
  region: string;
  location: LocationOption;
}
export interface ServerTypeOffer {
  name: string;
  description: string;
  cores: number;
  memoryGb: number;
  diskGb: number;
  arch: string;
  cpuType: string;
  prices: { location: string; monthlyCents: number }[];
}
export interface ProviderCatalog {
  locations: ProviderLocation[];
  serverTypes: ServerTypeOffer[];
}
export interface ProviderSshKey {
  /** The provider's id; at AWS `<region>/<name>`. */
  id: string;
  name: string;
  fingerprint: string;
}
export type ProvisionStatus =
  | "creating"
  | "booting"
  | "joining"
  | "ready"
  | "failed"
  | "deleting"
  | "resizing"
  | "pending_deletion";
export interface ProvisionedServer {
  id: string;
  /** A provider kind: `hetzner` or `aws`. */
  provider: string;
  providerId: string | null;
  name: string;
  location: LocationOption | null;
  serverType: string;
  monthlyPriceCents: number;
  status: ProvisionStatus;
  error: string | null;
  sshEnabled: boolean;
  ipv4: string | null;
  nodeId: string | null;
  createdAt: string;
}
export interface ProvisionedList {
  servers: ProvisionedServer[];
  connectedProviders: ConnectedProvider[];
}
export interface ProvisionRequest {
  location: string;
  serverType: string;
  name: string;
  ssh: boolean;
  sshKeyIds: string[];
  /** Root disk in GB, for providers that sell it apart from the type (EC2). */
  diskGb?: number;
}

const base = (ws: string) => `/api/workspaces/${enc(ws)}`;
const integration = (ws: string, kind: string) => `${base(ws)}/integrations/${enc(kind)}`;

export const provisionApi = {
  hetzner: (ws: string) => get<HetznerStatus>(integration(ws, "hetzner")),
  connectHetzner: (ws: string, token: string) =>
    request<HetznerStatus>("PUT", integration(ws, "hetzner"), {
      token,
    }),
  aws: (ws: string) => get<AwsStatus>(integration(ws, "aws")),
  connectAws: (ws: string, key: { accessKeyId: string; secretAccessKey: string }) =>
    request<AwsStatus>("PUT", integration(ws, "aws"), key),
  /** Limits and disconnecting work the same for every provider. */
  updateCaps: (ws: string, kind: string, caps: Partial<ProviderCaps>) => patch<ProviderStatus>(integration(ws, kind), caps),
  disconnect: (ws: string, kind: string) => del<ProviderStatus>(integration(ws, kind)),
  catalog: (ws: string, kind: string) => get<ProviderCatalog>(`${integration(ws, kind)}/catalog`),
  sshKeys: (ws: string, kind: string) => get<ProviderSshKey[]>(`${integration(ws, kind)}/ssh-keys`),
  /** `catalog` for Hetzner, kept for callers that only know that provider. */
  hetznerCatalog: (ws: string) => get<ProviderCatalog>(`${integration(ws, "hetzner")}/catalog`),
  servers: (ws: string) => get<ProvisionedList>(`${base(ws)}/servers`),
  provision: (ws: string, req: ProvisionRequest) => post<ProvisionedServer>(`${base(ws)}/servers/provision`, req),
  server: (id: string) => get<ProvisionedServer>(`/api/servers/${enc(id)}`),
  deleteServer: (id: string, force = false) => del<{ ok: true }>(`/api/servers/${enc(id)}${force ? "?force=true" : ""}`),
};

/** $4.52 from 452 cents. */
export const usd = (cents: number) => `$${(cents / 100).toFixed(2)}`;
/** A price converted from the provider's currency: ~$4.52. */
export const approxUsd = (cents: number) => `~${usd(cents)}`;

/** How the dashboard names a provider kind (`hetzner`, `aws`); an unknown kind is shown as it is. */
export function providerLabel(kind: string | null | undefined): string {
  switch (kind) {
    case "hetzner":
      return "Hetzner Cloud";
    case "aws":
      return "AWS EC2";
    default:
      return kind || "its provider";
  }
}
/** The short name, for running text: "deleted at AWS". */
export function providerName(kind: string | null | undefined): string {
  switch (kind) {
    case "hetzner":
      return "Hetzner";
    case "aws":
      return "AWS";
    default:
      return kind || "its provider";
  }
}

/** Only Hetzner's prices are converted (it bills in euros); AWS bills in USD. */
export const isApproximate = (kind: string | null | undefined) => kind === "hetzner";
/** A monthly price in USD as that provider bills it: `~$4.52` when converted, `$12.40` when exact. */
export const monthlyUsd = (cents: number, kind: string | null | undefined) =>
  isApproximate(kind) ? approxUsd(cents) : usd(cents);

/**
 * The root disk of providers that sell it apart from the server type (EC2).
 * Catalog prices assume the default size; each GB changes the monthly price by
 * `centsPerGb` (gp3), as `price_with_disk` does on the server.
 */
export const ROOT_DISK = { min: 20, default: 80, max: 16_384, centsPerGb: 8 } as const;
export const hasRootDisk = (kind: string | null | undefined) => kind === "aws";

/** A root disk size from a text field: whole GB within the allowed range, else null. */
export function parseDiskGb(text: string): number | null {
  const t = text.trim();
  if (!/^\d{1,6}$/.test(t)) return null;
  const n = Number(t);
  return n >= ROOT_DISK.min && n <= ROOT_DISK.max ? n : null;
}

/** A catalog price (at the default disk) for a root disk of `diskGb`; unchanged where the disk comes with the type. */
export const priceWithDisk = (kind: string | null | undefined, cents: number, diskGb: number) =>
  hasRootDisk(kind) ? cents + (diskGb - ROOT_DISK.default) * ROOT_DISK.centsPerGb : cents;

/** The steps a new server goes through, for the progress view. */
export const PROVISION_STEPS: { status: ProvisionStatus; label: string }[] = [
  { status: "creating", label: "Creating" },
  { status: "booting", label: "Booting" },
  { status: "joining", label: "Adding node" },
  { status: "ready", label: "Ready" },
];

/** Index of the current step (failed/deleting map to -1). */
export function stepIndex(status: ProvisionStatus): number {
  return PROVISION_STEPS.findIndex((s) => s.status === status);
}

/** Lowercase DNS label, as the server requires. */
export function validServerName(name: string): boolean {
  return /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/.test(name);
}

/** Types orderable in `location`, cheapest first, with that location's price. */
export function typesIn(types: ServerTypeOffer[], location: string) {
  return types
    .flatMap((t) => {
      const price = t.prices.find((p) => p.location === location);
      return price ? [{ ...t, monthlyCents: price.monthlyCents }] : [];
    })
    .sort((a, b) => a.monthlyCents - b.monthlyCents || a.cores - b.cores || a.name.localeCompare(b.name));
}

/** The connection to provider `kind` in a server list, when there is one. */
export const providerOf = (list: ProvisionedList | undefined, kind: string) =>
  list?.connectedProviders.find((p) => p.kind === kind);
/** The Hetzner connection in a server list, when there is one. */
export const hetznerOf = (list: ProvisionedList | undefined) => providerOf(list, "hetzner");

/** Where an AWS key id (`eu-west-1/laptop`) lives. */
export const keyRegion = (id: string) => id.split("/", 1)[0] ?? "";

/** Where a new server goes by default: the provider's usual first pick when it is on offer, else the first location listed. */
const DEFAULT_LOCATION: Record<string, string> = { hetzner: "hetzner:fsn1", aws: "aws:us-east-1" };
export function defaultLocation(kind: string, locations: ProviderLocation[]): string | null {
  const preferred = DEFAULT_LOCATION[kind];
  return (locations.find((l) => l.location.id === preferred) ?? locations[0])?.location.id ?? null;
}
