// Moving services between servers with their data, and migrating a worker
// server to a new one (crates/server/src/migrate).

import { get, post, type LocationOption } from "./api";

const enc = encodeURIComponent;

export interface ProgressStep {
  label: string;
  status: "running" | "done" | "failed";
  detail?: string;
}

export type MoveStatus = "pending" | "running" | "failed" | "aborting" | "aborted" | "done";

export interface InstanceMove {
  id: string;
  migrationId: string | null;
  serviceInstanceId: string;
  service: string;
  environment: string;
  project: string;
  /** primary: the service moves; replicas: only its replicas on the old server. */
  kind: "primary" | "replicas";
  fromNodeId: string | null;
  fromName: string | null;
  toNodeId: string | null;
  toName: string | null;
  status: MoveStatus;
  phase: "stop" | "stopping" | "copy" | "deploy" | "wait" | "done";
  deploymentId: string | null;
  copiedBytes: number;
  steps: ProgressStep[];
  error: string | null;
  createdAt: string;
  finishedAt: string | null;
}

export type MigrationStatus = "running" | "failed" | "aborting" | "aborted" | "done";

export type PromotePhase = "bundle" | "install" | "verify" | "switch";

export interface ServerMigration {
  id: string;
  workspaceId: string;
  sourceNodeId: string | null;
  sourceServerId: string | null;
  sourceName: string;
  targetLocation: LocationOption | null;
  targetServerType: string;
  targetServerId: string | null;
  targetName: string | null;
  targetNodeId: string | null;
  status: MigrationStatus;
  step: "create" | "join" | "move" | "promote" | "finish";
  /** The main server (control plane) is being moved. */
  main: boolean;
  /** Within `promote`: bundle (maintenance on, copying), install, verify, switch (handover). */
  promotePhase: PromotePhase | null;
  /** Where the dashboard will be after the move; set once promote starts. */
  newPublicUrl: string | null;
  steps: ProgressStep[];
  error: string | null;
  /** The old server waits for the user to delete it. */
  sourcePendingDeletion: boolean;
  createdAt: string;
  finishedAt: string | null;
  moves: InstanceMove[];
  /** TCP proxies' addresses on the new server. */
  tcpProxies: { service: string; host: string | null; publicPort: number; targetPort: number }[];
}

export interface MigrationPreview {
  nodeId: string;
  name: string;
  main: boolean;
  /** Set when the main server can't be migrated: the reason. */
  mainBlocker: string | null;
  /** The dashboard's URL now. */
  currentUrl: string;
  /** The dashboard is reached by IP: its URL changes to the new server's IP. */
  ipBased: boolean;
  /** Hostnames whose DNS must point at the new server. */
  dashboardHosts: string[];
  /** Set when the server is linked to Hetzner (required to migrate). */
  serverId: string | null;
  workspaceId: string | null;
  location: LocationOption | null;
  services: {
    instanceId: string;
    service: string;
    environment: string;
    project: string;
    replicaOnly: boolean;
    volumes: { id: string; mountPath: string; usedBytes: number | null }[];
    tcpAddress: string | null;
  }[];
  totalVolumeBytes: number;
  activeMigrationId: string | null;
}

export const migrateApi = {
  preview: (nodeId: string) => get<MigrationPreview>(`/api/nodes/${enc(nodeId)}/migration-preview`),
  migrations: (nodeId: string) => get<ServerMigration[]>(`/api/nodes/${enc(nodeId)}/migrations`),
  migrate: (nodeId: string, req: { location: string; serverType: string }) =>
    post<ServerMigration>(`/api/nodes/${enc(nodeId)}/migrate`, req),
  migration: (id: string) => get<ServerMigration>(`/api/server-migrations/${enc(id)}`),
  abort: (id: string) => post<ServerMigration>(`/api/server-migrations/${enc(id)}/abort`),
  moveInstance: (instanceId: string, nodeId: string) =>
    post<InstanceMove>(`/api/service-instances/${enc(instanceId)}/move`, { nodeId }),
  move: (id: string) => get<InstanceMove>(`/api/instance-moves/${enc(id)}`),
  abortMove: (id: string) => post<InstanceMove>(`/api/instance-moves/${enc(id)}/abort`),
};

/** Still changing: worth polling. */
export function migrationActive(status: MigrationStatus | MoveStatus | undefined): boolean {
  return status === "running" || status === "aborting" || status === "pending";
}

export interface Maintenance {
  mode: "maintenance" | "moved" | null;
  message: string | null;
  newUrl: string | null;
}

/** The dashboard's maintenance state (public). Never throws: a 503 `moved`
 *  answer from the old server counts as moved. */
export async function getMaintenance(): Promise<Maintenance> {
  const none: Maintenance = { mode: null, message: null, newUrl: null };
  try {
    const res = await fetch("/api/maintenance", { cache: "no-store", credentials: "same-origin" });
    if (res.ok) return (await res.json()) as Maintenance;
    const body = (await res.json().catch(() => null)) as {
      error?: { code?: string; message?: string; movedTo?: string };
    } | null;
    if (body?.error?.code === "moved") {
      return { mode: "moved", message: body.error.message ?? null, newUrl: body.error.movedTo ?? null };
    }
  } catch {
    /* unreachable: not our call */
  }
  return none;
}

export const PROMOTE_LABELS: Record<PromotePhase, string> = {
  bundle: "Copying the control plane (dashboard is read-only)",
  install: "Installing the control plane on the new server",
  verify: "Checking the new control plane answers",
  switch: "Handing over: servers switch, DNS is repointed",
};
