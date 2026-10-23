// Typed client for the modula-server API. Same-origin cookies only; the browser
// sends `Origin` on mutations, which the server checks (CSRF).

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

export async function parseError(res: Response): Promise<ApiError> {
  let code = "http_" + res.status;
  let message = res.statusText || "Request failed";
  try {
    const body = (await res.json()) as { error?: { code?: string; message?: string } };
    if (body.error?.code) code = body.error.code;
    if (body.error?.message) message = body.error.message;
  } catch {
    /* non-JSON error body */
  }
  return new ApiError(res.status, code, message);
}

type SudoHandler = () => Promise<boolean>;
let sudoHandler: SudoHandler | null = null;

/** The app registers a dialog that re-authenticates; requests that hit
 *  `sudo_required` wait for it and retry once. */
export function setSudoHandler(h: SudoHandler | null) {
  sudoHandler = h;
}

export async function request<T>(method: string, path: string, body?: unknown, retried = false): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    const err = await parseError(res);
    if (err.code === "sudo_required" && !retried && sudoHandler && (await sudoHandler())) {
      return request<T>(method, path, body, true);
    }
    throw err;
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export const get = <T>(p: string) => request<T>("GET", p);
export const post = <T>(p: string, b?: unknown) => request<T>("POST", p, b ?? {});
export const patch = <T>(p: string, b: unknown) => request<T>("PATCH", p, b);
export const del = <T>(p: string) => request<T>("DELETE", p);

// ---- types (mirror crates/server/src/api) ----

export type Role = "admin" | "member" | "deployer" | "viewer";
export interface Membership {
  id: string;
  name: string;
  role: Role;
}
export interface Me {
  user: { id: string; email: string; name: string; isInstanceAdmin: boolean };
  workspaces: Membership[];
}
export interface InstanceStatus {
  setupRequired: boolean;
  version: string;
}
export interface LoginResponse {
  mfaRequired: boolean;
  me?: Me;
}
export interface Security {
  totpEnabled: boolean;
  backupCodesRemaining: number;
  sudoUntil: string | null;
}
/** A personal API token (`Authorization: Bearer mdt_…`), e.g. for the CLI. */
export interface ApiToken {
  id: string;
  name: string;
  /** null = every workspace the user belongs to. */
  workspaceId: string | null;
  workspaceName: string | null;
  lastUsedAt: string | null;
  expiresAt: string | null;
  createdAt: string;
}
export interface SessionInfo {
  id: string;
  ip: string | null;
  userAgent: string | null;
  createdAt: string;
  lastSeenAt: string;
  current: boolean;
}

export interface Project {
  id: string;
  workspaceId: string;
  name: string;
  description: string;
  createdAt: string;
}
export interface Environment {
  id: string;
  name: string;
  createdAt: string;
}
export interface ServiceSummary {
  id: string;
  name: string;
  icon: string | null;
  /** Database template the service was created from, if any. */
  template?: string | null;
}
export type DeploymentStatus = "queued" | "building" | "deploying" | "active" | "failed" | "crashed" | "removed" | "cancelled";
export interface InstanceSummary {
  id: string;
  serviceId: string;
  environmentId: string;
  nodeId: string | null;
  latestDeploymentId: string | null;
  latestDeploymentStatus: DeploymentStatus | null;
  domain: string | null;
  sourceType: "image" | "repo" | null;
  repo: string | null;
  image?: string | null;
  /** Detected framework/runtime (e.g. "nextjs", "postgres"), used for the card icon. */
  stack?: string | null;
  /** Ids of the services this instance references through `${{service.NAME}}`. */
  dependsOn?: string[];
  template?: string | null;
  /** Mount path of the attached volume, if any. */
  volume?: { id: string; mountPath: string; createdAt: string } | null;
}
export interface ProjectDetail extends Project {
  environments: Environment[];
  services: ServiceSummary[];
  instances: InstanceSummary[];
}
export interface SyncResult {
  created: number;
  updated: number;
}
export interface ActivityChange {
  kind: "variable" | "config" | "deployment" | "service";
  op: "added" | "updated" | "removed" | "created" | "deleted" | "status";
  /** Variable name or config field. */
  key: string | null;
  /** Deployment status. */
  status: string | null;
  deploymentId: string | null;
  message: string | null;
  at: string;
}
export interface ActivityGroup {
  id: string;
  serviceId: string | null;
  serviceName: string | null;
  actor: { name: string; avatarUrl: string | null } | null;
  /** Newest change in the group. */
  at: string;
  changes: ActivityChange[];
}
export type ActivityKind = "deployment" | "variable" | "config";
export type RestartPolicy = "on_failure" | "always" | "never";
export type ServiceSource = { type: "image"; image: string } | { type: "repo"; repo: string; branch: string; rootDir?: string };
export type Builder = "railpack" | "dockerfile";
export interface ServiceConfig {
  source: ServiceSource;
  builder?: Builder;
  dockerfilePath?: string;
  buildCommand?: string;
  autoDeploy?: boolean;
  watchPaths?: string[];
  startCommand?: string[];
  port: number;
  healthcheckPath?: string;
  healthcheckTimeoutSecs: number;
  restartPolicy: RestartPolicy;
  /** on_failure only: stop restarting after this many tries (unset = never give up). */
  restartPolicyMaxRetries?: number;
  /** argv run once from the new image before it starts (e.g. migrations). */
  preDeployCommand?: string[];
  /** SIGTERM → SIGKILL grace for a retired container, 0–300 (unset = 10s default). */
  drainingSeconds?: number;
  memoryMb?: number;
  cpuMillis?: number;
  /** Each replica runs on a different server. */
  replicas?: number;
}
export interface Location {
  id: string;
  label: string;
  provider: string;
  countryCode: string;
}
export interface LocationOption extends Location {
  city: string;
}
export type InstanceState = "online" | "offline" | "deploying" | "crashed";
export interface Instance {
  id: string;
  serviceId: string;
  serviceName: string;
  environmentId: string;
  environmentName: string;
  projectId: string;
  nodeId: string | null;
  config: ServiceConfig;
  /** The server running it. */
  location?: Location | null;
  status?: InstanceState;
  stack?: string | null;
  template?: string | null;
  volume?: Volume | null;
  tcpProxy?: TcpProxy | null;
  updatedAt: string;
}
export interface Volume {
  id: string;
  mountPath: string;
  createdAt: string;
}
export interface TcpProxy {
  id: string;
  host: string;
  publicPort: number;
  targetPort: number;
}
export type TemplateId = "postgres" | "mysql" | "redis" | "mongo";
export interface Template {
  id: TemplateId;
  name: string;
  description: string;
  image: string;
  defaultName: string;
  /** Variables a client connects with, e.g. DATABASE_URL. */
  connect: { label: string; variable: string }[];
}
export type BackupProvider = "s3" | "r2" | "b2" | "minio";
export interface BackupTarget {
  id: string;
  name: string;
  provider: BackupProvider;
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  createdAt: string;
}
export interface BackupTargetInput {
  name: string;
  provider: BackupProvider;
  endpoint: string;
  region: string;
  bucket: string;
  prefix?: string;
  accessKeyId: string;
  secretAccessKey: string;
}
export interface BackupSchedule {
  enabled: boolean;
  targetId: string;
  hourUtc: number;
  retention: { dailyDays: number; weeklyDays: number; monthlyDays: number };
}
export type BackupStatus = "running" | "completed" | "failed" | "deleted";
export interface Backup {
  id: string;
  status: BackupStatus;
  kind: "scheduled" | "manual";
  sizeBytes: number | null;
  startedAt: string;
  finishedAt: string | null;
  error: string | null;
  targetName: string;
}
export interface BackupsInfo {
  schedule: BackupSchedule | null;
  backups: Backup[];
}
export interface SelfBackup {
  enabled: boolean;
  targetId: string | null;
  hourUtc: number;
}
export interface VariableInfo {
  id: string;
  name: string;
  sealed: boolean;
  updatedAt: string;
  /** What the value points at, e.g. `postgres.DATABASE_URL`, `shared.API_KEY`. */
  references?: string[];
}
export interface VariablePatch {
  set?: Record<string, string>;
  sealed?: string[];
  delete?: string[];
}
/** Names only, for `${{` autocomplete. */
export interface VariableNames {
  shared: string[];
  services: { serviceId: string; serviceName: string; names: string[]; platform: string[] }[];
}
export interface PendingChanges {
  count: number;
  services: { serviceId: string; serviceName: string; instanceId: string; changes: ActivityChange[] }[];
}
export interface Deployment {
  id: string;
  serviceInstanceId: string;
  nodeId: string | null;
  status: DeploymentStatus;
  image: string;
  imageDigest: string | null;
  commitSha: string | null;
  commitMessage: string | null;
  commitAuthor: string | null;
  /** What was deployed: `owner/name@branch`, the image, or "Uploaded via CLI". */
  source: string;
  buildStartedAt: string | null;
  buildFinishedAt: string | null;
  sourceKind?: "github" | "image" | "upload";
  /** Uploads (`modula-deploy up`): the local branch, whether uncommitted changes were included,
   * and whether the files are still kept (so it can be redeployed or rolled back to). */
  sourceBranch?: string | null;
  sourceDirty?: boolean | null;
  uploadAvailable?: boolean | null;
  /** manual | push | rollback | redeploy | restart | upload */
  trigger: string;
  commitAuthorAvatar?: string | null;
  error: string | null;
  createdBy: string | null;
  createdAt: string;
  updatedAt: string;
  finishedAt: string | null;
}
export interface DeploymentEvent {
  phase: string;
  message: string;
  at: string;
}
export interface DeploymentDetail extends Deployment {
  events: DeploymentEvent[];
}
export interface DnsRecord {
  type: "CNAME" | "A" | "TXT";
  name: string;
  value: string;
}
export interface Domain {
  id: string;
  hostname: string;
  kind: "generated" | "custom";
  /** Generated domains are always verified; custom ones route only once verified. */
  status: "pending" | "verified" | "failed";
  /** null = the service's port. */
  targetPort: number | null;
  lastError: string | null;
  /** Whether the CNAME/A record pointed at us on the last check (null = unknown). */
  dnsTargetOk: boolean | null;
  dnsRecords: DnsRecord[];
  rateLimitPerMinute: number | null;
  /** False until the edge's Caddy build has the rate-limit module. */
  rateLimitEnforced: boolean;
  providerNote?: string;
  checkedAt: string | null;
  verifiedAt: string | null;
  createdAt: string;
}
export interface NodeInfo {
  id: string;
  name: string;
  status: "online" | "offline" | "revoked";
  nodeVersion: string | null;
  runtime: string | null;
  info: { hostname?: string; os?: string; arch?: string; cpus?: number; memoryBytes?: number };
  desiredGeneration: number;
  appliedGeneration: number;
  lastSeenAt: string | null;
  certExpiresAt: string;
  createdAt: string;
  connected: boolean;
  location?: Location | null;
}
export interface JoinToken {
  token: string;
  expiresAt: string;
  server: string;
  fingerprint: string;
  /** Installs Docker, Caddy and modula-node on a fresh server, then joins. */
  command: string;
  /** The join step alone, for a server that already runs modula-node. */
  nodeCommand: string;
}

// ---- database browser (Postgres services) ----

export interface DbTable {
  schema: string;
  name: string;
  rowEstimate: number;
}
export interface DbColumn {
  name: string;
  type: string;
  nullable: boolean;
  primaryKey: boolean;
}
export interface DbRows {
  columns: DbColumn[];
  rows: (string | null)[][];
  total: number;
  page: number;
  pageSize: number;
}
export interface DbQueryResult {
  columns: { name: string; type: string }[];
  rows: (string | null)[][];
  rowCount: number;
  command: string;
  durationMs: number;
  truncated: boolean;
}
export interface DbRowsQuery {
  table: string;
  page: number;
  pageSize: number;
  orderBy?: string;
  dir?: "asc" | "desc";
}
export interface DbTableStats {
  name: string;
  rows: number;
  dataBytes: number;
  indexBytes: number;
  seqScans: number;
  idxScans: number;
  deadRows: number;
  deadPct: number;
  lastVacuum: string | null;
  xidAge: number;
}
export interface DbStats {
  connections: { used: number; max: number; active: number; idle: number; idleInTransaction: number };
  cacheHitRatio: number;
  sizes: { database: number; tables: number; indexes: number; system: number; other: number; wal: number };
  tables: DbTableStats[];
  unusedIndexes: { name: string; table: string; bytes: number; scans: number }[];
  pgStatStatements: boolean;
  topQueries: { query: string; calls: number; meanMs: number; totalMs: number }[];
}
export interface DbExtension {
  name: string;
  version: string;
  comment: string;
}
export interface DbExtensions {
  installed: DbExtension[];
  available: DbExtension[];
}
export interface DbCredentials {
  username: string;
  password: string;
  database: string;
  host: string;
  port: number;
  privateUrl: string;
  publicUrl: string | null;
}

/** True when the server doesn't have the database endpoints yet. */
export const isNotAvailable = (e: unknown) => e instanceof ApiError && (e.status === 404 || e.status === 501);

const enc = encodeURIComponent;

export const api = {
  instance: () => get<InstanceStatus>("/api/instance"),
  me: () => get<Me>("/api/auth/me"),
  setup: (b: { setupToken: string; email: string; name: string; password: string; workspaceName?: string }) =>
    post<Me>("/api/auth/setup", b),
  login: (b: { email: string; password: string }) => post<LoginResponse>("/api/auth/login", b),
  mfa: (b: { totpCode: string } | { backupCode: string }) => post<Me>("/api/auth/mfa", b),
  sudo: (b: { password: string } | { totpCode: string } | { backupCode: string }) =>
    post<{ sudoUntil: string }>("/api/auth/sudo", b),
  logout: () => post<{ ok: true }>("/api/auth/logout"),
  security: () => get<Security>("/api/account/security"),
  totpBegin: () => post<{ otpauthUrl: string; secret: string }>("/api/account/totp"),
  totpConfirm: (code: string) => post<{ backupCodes: string[] }>("/api/account/totp/confirm", { code }),
  totpDisable: () => del<{ ok: true }>("/api/account/totp"),
  backupCodes: () => post<{ backupCodes: string[] }>("/api/account/backup-codes"),
  changePassword: (newPassword: string) =>
    post<{ ok: true; sessionsRevoked: number }>("/api/account/password", { newPassword }),
  sessions: () => get<SessionInfo[]>("/api/account/sessions"),
  revokeSession: (id: string) => del<{ ok: true }>(`/api/account/sessions/${encodeURIComponent(id)}`),
  apiTokens: () => get<ApiToken[]>("/api/account/tokens"),
  /** Needs sudo (the request layer asks). The token is returned only here. */
  createApiToken: (b: { name: string; workspaceId?: string; expiresInDays?: number }) =>
    post<ApiToken & { token: string }>("/api/account/tokens", b),
  revokeApiToken: (id: string) => del<{ ok: true }>(`/api/account/tokens/${encodeURIComponent(id)}`),
  createWorkspace: (name: string) => post<{ id: string; name: string }>("/api/workspaces", { name }),
  updateWorkspace: (id: string, name: string) => patch<{ id: string; name: string }>(`/api/workspaces/${enc(id)}`, { name }),
  deleteWorkspace: (id: string) => del<{ ok: true }>(`/api/workspaces/${enc(id)}`),

  projects: (workspace: string) => get<Project[]>(`/api/workspaces/${enc(workspace)}/projects`),
  createProject: (workspace: string, b: { name: string; description?: string }) =>
    post<ProjectDetail>(`/api/workspaces/${enc(workspace)}/projects`, b),
  project: (id: string) => get<ProjectDetail>(`/api/projects/${enc(id)}`),
  updateProject: (id: string, b: { name?: string; description?: string }) => patch<Project>(`/api/projects/${enc(id)}`, b),
  deleteProject: (id: string) => del<{ ok: true }>(`/api/projects/${enc(id)}`),
  createEnvironment: (project: string, b: { name: string; cloneFrom?: string; copyVariables?: boolean }) =>
    post<Environment>(`/api/projects/${enc(project)}/environments`, b),
  syncEnvironment: (target: string, fromEnvironmentId: string) =>
    post<SyncResult>(`/api/environments/${enc(target)}/sync`, { fromEnvironmentId }),
  activity: (project: string, q: { environmentId?: string; serviceId?: string; kind?: ActivityKind }) => {
    const params = new URLSearchParams();
    if (q.environmentId) params.set("environmentId", q.environmentId);
    if (q.serviceId) params.set("serviceId", q.serviceId);
    if (q.kind) params.set("kind", q.kind);
    return get<{ items: ActivityGroup[] }>(`/api/projects/${enc(project)}/activity?${params}`);
  },
  deleteEnvironment: (id: string) => del<{ ok: true }>(`/api/environments/${enc(id)}`),

  createService: (
    project: string,
    b: { name: string; config: Partial<ServiceConfig> & { source: ServiceConfig["source"] } } | { name: string; template: TemplateId },
  ) =>
    post<{ id: string; name: string; instances: Instance[] }>(`/api/projects/${enc(project)}/services`, b),
  deleteService: (id: string) => del<{ ok: true }>(`/api/services/${enc(id)}`),
  serviceInstance: (id: string) => get<Instance>(`/api/service-instances/${enc(id)}`),
  updateInstance: (id: string, config: ServiceConfig) => patch<Instance>(`/api/service-instances/${enc(id)}`, { config }),

  variables: (instance: string) => get<VariableInfo[]>(`/api/service-instances/${enc(instance)}/variables`),
  updateVariables: (instance: string, b: VariablePatch) =>
    patch<VariableInfo[]>(`/api/service-instances/${enc(instance)}/variables`, b),
  sharedVariables: (environment: string) => get<VariableInfo[]>(`/api/environments/${enc(environment)}/variables`),
  updateSharedVariables: (environment: string, b: VariablePatch) =>
    patch<VariableInfo[]>(`/api/environments/${enc(environment)}/variables`, b),
  variableNames: (environment: string) => get<VariableNames>(`/api/environments/${enc(environment)}/variable-names`),
  pending: (environment: string) => get<PendingChanges>(`/api/environments/${enc(environment)}/pending`),
  deployPending: (environment: string, serviceIds?: string[]) =>
    post<{ deployments: { serviceId: string; deploymentId: string }[] }>(`/api/environments/${enc(environment)}/deploy`, {
      serviceIds,
    }),
  revealVariable: (id: string) => post<{ name: string; value: string }>(`/api/variables/${enc(id)}/reveal`),

  deployments: (instance: string) => get<Deployment[]>(`/api/service-instances/${enc(instance)}/deployments`),
  deploy: (instance: string) => post<DeploymentDetail>(`/api/service-instances/${enc(instance)}/deployments`),
  deployment: (id: string) => get<DeploymentDetail>(`/api/deployments/${enc(id)}`),
  redeployDeployment: (id: string) => post<DeploymentDetail>(`/api/deployments/${enc(id)}/redeploy`),
  rollbackDeployment: (id: string) => post<DeploymentDetail>(`/api/deployments/${enc(id)}/rollback`),
  restartDeployment: (id: string) => post<DeploymentDetail>(`/api/deployments/${enc(id)}/restart`),
  removeDeployment: (id: string) => del<DeploymentDetail>(`/api/deployments/${enc(id)}`),
  deploymentLogsUrl: (id: string, tail = 500, kind: "deploy" | "build" = "deploy") =>
    `/api/deployments/${enc(id)}/logs?${kind === "build" ? "kind=build&" : ""}tail=${tail}&follow=true`,

  domains: (instance: string) => get<Domain[]>(`/api/service-instances/${enc(instance)}/domains`),
  generateDomain: (instance: string) => post<Domain>(`/api/service-instances/${enc(instance)}/domains`),
  addCustomDomain: (instance: string, body: { hostname: string; targetPort: number | null }) =>
    post<Domain>(`/api/service-instances/${enc(instance)}/custom-domains`, body),
  checkDomain: (id: string) => post<Domain>(`/api/domains/${enc(id)}/check`),
  updateDomain: (id: string, body: { targetPort: number | null; rateLimitPerMinute: number | null }) =>
    patch<Domain>(`/api/domains/${enc(id)}`, body),
  deleteDomain: (id: string) => del<{ ok: true }>(`/api/domains/${enc(id)}`),

  templates: () => get<Template[]>("/api/templates"),
  createVolume: (instance: string, mountPath: string) => post<Instance>(`/api/service-instances/${enc(instance)}/volume`, { mountPath }),
  deleteVolume: (id: string) => del<{ ok: true }>(`/api/volumes/${enc(id)}`),
  createTcpProxy: (instance: string, targetPort: number) =>
    post<Instance>(`/api/service-instances/${enc(instance)}/tcp-proxy`, { targetPort }),
  deleteTcpProxy: (id: string) => del<{ ok: true }>(`/api/tcp-proxies/${enc(id)}`),

  backupTargets: (workspace: string) => get<BackupTarget[]>(`/api/workspaces/${enc(workspace)}/backup-targets`),
  createBackupTarget: (workspace: string, b: BackupTargetInput) =>
    post<BackupTarget>(`/api/workspaces/${enc(workspace)}/backup-targets`, b),
  deleteBackupTarget: (id: string) => del<{ ok: true }>(`/api/backup-targets/${enc(id)}`),
  backups: (instance: string) => get<BackupsInfo>(`/api/service-instances/${enc(instance)}/backups`),
  saveBackupSchedule: (instance: string, b: BackupSchedule) =>
    request<BackupsInfo>("PUT", `/api/service-instances/${enc(instance)}/backup-schedule`, b),
  backupNow: (instance: string) => post<Backup>(`/api/service-instances/${enc(instance)}/backups`),
  restoreBackup: (id: string) => post<{ ok: true }>(`/api/backups/${enc(id)}/restore`),
  deleteBackup: (id: string) => del<{ ok: true }>(`/api/backups/${enc(id)}`),
  selfBackup: () => get<SelfBackup>("/api/instance/self-backup"),
  saveSelfBackup: (b: SelfBackup) => request<SelfBackup>("PUT", "/api/instance/self-backup", b),

  dbTables: (i: string) => get<{ tables: DbTable[] }>(`/api/service-instances/${enc(i)}/database/tables`),
  dbRows: (i: string, q: DbRowsQuery) => {
    const p = new URLSearchParams({ table: q.table, page: String(q.page), pageSize: String(q.pageSize) });
    if (q.orderBy) {
      p.set("orderBy", q.orderBy);
      p.set("dir", q.dir ?? "asc");
    }
    return get<DbRows>(`/api/service-instances/${enc(i)}/database/rows?${p}`);
  },
  dbQuery: (i: string, sql: string) => post<DbQueryResult>(`/api/service-instances/${enc(i)}/database/query`, { sql }),
  dbInsertRow: (i: string, table: string, values: Record<string, string | null>) =>
    post<unknown>(`/api/service-instances/${enc(i)}/database/rows`, { table, values }),
  dbUpdateRow: (i: string, table: string, key: Record<string, string | null>, values: Record<string, string | null>) =>
    patch<unknown>(`/api/service-instances/${enc(i)}/database/rows`, { table, key, values }),
  dbDeleteRow: (i: string, table: string, key: Record<string, string | null>) =>
    request<unknown>("DELETE", `/api/service-instances/${enc(i)}/database/rows`, { table, key }),
  dbStats: (i: string) => get<DbStats>(`/api/service-instances/${enc(i)}/database/stats`),
  dbVacuum: (i: string, table: string) => post<unknown>(`/api/service-instances/${enc(i)}/database/vacuum`, { table }),
  dbExtensions: (i: string) => get<DbExtensions>(`/api/service-instances/${enc(i)}/database/extensions`),
  dbSetExtension: (i: string, name: string, action: "install" | "uninstall") =>
    post<unknown>(`/api/service-instances/${enc(i)}/database/extensions`, { name, action }),
  dbCredentials: (i: string) => get<DbCredentials>(`/api/service-instances/${enc(i)}/database/credentials`),
  dbRegeneratePassword: (i: string) => post<DbCredentials>(`/api/service-instances/${enc(i)}/database/password`),

  nodes: () => get<NodeInfo[]>("/api/nodes"),
  createJoinToken: () => post<JoinToken>("/api/nodes/join-tokens"),
  locations: () => get<LocationOption[]>("/api/locations"),
  setNodeLocation: (id: string, location: string | null) => patch<NodeInfo>(`/api/nodes/${enc(id)}`, { location }),
  revokeNode: (id: string) => del<{ ok: true }>(`/api/nodes/${enc(id)}`),
};
