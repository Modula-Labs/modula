// Hetzner integration and servers created through it (crates/server/src/provision,
// crates/server/src/integrations/hetzner).

import { del, get, patch, post, request, type LocationOption } from "./api";

const enc = encodeURIComponent;

export interface HetznerCaps {
  maxServers: number;
  /** Gross euro cents per month. */
  maxMonthlyCents: number;
}
export interface HetznerUsage {
  servers: number;
  monthlyCents: number;
}
export interface HetznerStatus {
  connected: boolean;
  connectedAt: string | null;
  connectedBy: string | null;
  caps: HetznerCaps;
  usage: HetznerUsage;
  tokenUrl: string;
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
export interface HetznerCatalog {
  locations: ProviderLocation[];
  serverTypes: ServerTypeOffer[];
}
export interface HetznerSshKey {
  id: number;
  name: string;
  fingerprint: string;
}
export type ProvisionStatus = "creating" | "booting" | "joining" | "ready" | "failed" | "deleting";
export interface ProvisionedServer {
  id: string;
  provider: "hetzner";
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
  hetznerConnected: boolean;
  caps: HetznerCaps;
  usage: HetznerUsage;
}
export interface ProvisionRequest {
  location: string;
  serverType: string;
  name: string;
  ssh: boolean;
  sshKeyIds: number[];
}

const base = (ws: string) => `/api/workspaces/${enc(ws)}`;

export const provisionApi = {
  hetzner: (ws: string) => get<HetznerStatus>(`${base(ws)}/integrations/hetzner`),
  connectHetzner: (ws: string, token: string) =>
    request<HetznerStatus>("PUT", `${base(ws)}/integrations/hetzner`, {
      token,
    }),
  updateHetznerCaps: (ws: string, caps: Partial<HetznerCaps>) => patch<HetznerStatus>(`${base(ws)}/integrations/hetzner`, caps),
  disconnectHetzner: (ws: string) => del<HetznerStatus>(`${base(ws)}/integrations/hetzner`),
  hetznerCatalog: (ws: string) => get<HetznerCatalog>(`${base(ws)}/integrations/hetzner/catalog`),
  hetznerSshKeys: (ws: string) => get<HetznerSshKey[]>(`${base(ws)}/integrations/hetzner/ssh-keys`),
  servers: (ws: string) => get<ProvisionedList>(`${base(ws)}/servers`),
  provision: (ws: string, req: ProvisionRequest) => post<ProvisionedServer>(`${base(ws)}/servers/provision`, req),
  server: (id: string) => get<ProvisionedServer>(`/api/servers/${enc(id)}`),
  deleteServer: (id: string, force = false) => del<{ ok: true }>(`/api/servers/${enc(id)}${force ? "?force=true" : ""}`),
};

/** €4.52 from 452 cents. */
export const euros = (cents: number) => `€${(cents / 100).toFixed(2)}`;

/** The steps a new server goes through, for the progress view. */
export const PROVISION_STEPS: { status: ProvisionStatus; label: string }[] = [
  { status: "creating", label: "Creating" },
  { status: "booting", label: "Booting" },
  { status: "joining", label: "Joining" },
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
