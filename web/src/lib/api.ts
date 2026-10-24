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
    const body = (await res.json()) as {
      error?: { code?: string; message?: string };
    };
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

/** For WebSockets, whose failed handshake carries no error code: make sure
 *  the session is in sudo mode first, asking to re-authenticate if not.
 *  False when the user cancelled. */
export async function ensureSudo(): Promise<boolean> {
  const s = await get<Security>("/api/account/security");
  if (s.sudoUntil && Date.parse(s.sudoUntil) > Date.now() + 5_000) return true;
  return sudoHandler ? sudoHandler() : false;
}

/** `wss://<this host>/api/deployments/<id>/exec?…` for an interactive shell. */
export function execUrl(deploymentId: string, size: { cols: number; rows: number }, loc: { protocol: string; host: string } = window.location): string {
  const scheme = loc.protocol === "https:" ? "wss" : "ws";
  const q = new URLSearchParams({ tty: "true", cols: String(size.cols), rows: String(size.rows) });
  return `${scheme}://${loc.host}/api/deployments/${encodeURIComponent(deploymentId)}/exec?${q}`;
}

export async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  retried = false,
): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: "same-origin",
    headers:
      body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) {
    const err = await parseError(res);
    if (
      err.code === "sudo_required" &&
      !retried &&
      sudoHandler &&
      (await sudoHandler())
    ) {
      return request<T>(method, path, body, true);
    }
    throw err;
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export const get = <T>(p: string) => request<T>("GET", p);
export const post = <T>(p: string, b?: unknown) =>
  request<T>("POST", p, b ?? {});
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
/** A workspace member (`id` is the membership's, not the user's). */
export interface Member {
  id: string;
  userId: string;
  name: string;
  email: string;
  role: Role;
  joinedAt: string;
  twoFactor: boolean;
}
/** A pending invite link; the link itself is only returned when created. */
export interface Invite {
  id: string;
  role: Role;
  createdByName: string | null;
  createdAt: string;
  expiresAt: string;
}
/** What an invite link offers, shown before joining. */
export interface InvitePreview {
  workspaceName: string;
  role: Role;
  invitedBy: string | null;
  expiresAt: string;
}
export interface InviteJoined {
  workspaceId: string;
  me: Me;
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
/** A project on the projects page: its first environment's services. */
export interface ProjectCard extends Project {
  environment: string | null;
  services: {
    id: string;
    name: string;
    stack: string | null;
    image: string | null;
    repo: string | null;
    online: boolean;
  }[];
}
export interface Environment {
  id: string;
  name: string;
  createdAt: string;
  /** The project's first environment (production): can't be deleted. */
  isDefault: boolean;
}
export interface ServiceSummary {
  id: string;
  name: string;
  icon: string | null;
  /** Database template the service was created from, if any. */
  template?: string | null;
  /** The gallery app it was deployed with (see `AppTemplate`). */
  appTemplate?: string | null;
}
export type DeploymentStatus =
  | "queued"
  | "building"
  | "deploying"
  | "active"
  | "failed"
  | "crashed"
  | "removed"
  | "cancelled";
export interface InstanceSummary {
  id: string;
  serviceId: string;
  environmentId: string;
  nodeId: string | null;
  latestDeploymentId: string | null;
  latestDeploymentStatus: DeploymentStatus | null;
  domain: string | null;
  sourceType: "image" | "repo" | "local" | "r2" | null;
  repo: string | null;
  /** The branch a repo source deploys from. */
  branch?: string | null;
  image?: string | null;
  /** Detected framework/runtime (e.g. "nextjs", "postgres"), used for the card icon. */
  stack?: string | null;
  /** Ids of the services this instance references through `${{service.NAME}}`. */
  dependsOn?: string[];
  template?: string | null;
  /** Mount path of the attached volume, if any. */
  volume?: { id: string; mountPath: string; createdAt: string } | null;
  /** An R2 bucket service's bucket in this environment (null = none yet). */
  bucket?: string | null;
  /** The configured cron schedule, if the service is a cron job. */
  cronSchedule: string | null;
  /** The next due time (RFC3339) of the deployed schedule. */
  cronNextAt: string | null;
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
export type ServiceSource =
  | { type: "image"; image: string }
  | { type: "repo"; repo: string; branch: string; rootDir?: string }
  | { type: "local"; rootDir?: string }
  /** A Cloudflare R2 bucket: nothing runs or deploys. */
  | { type: "r2" };
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
  /** 5-field cron expression in UTC (or `@daily` etc.): the service runs to completion at each due time instead of continuously. */
  cronSchedule?: string;
  /** Seconds a cron run may take before it is stopped, 1–86400 (unset = 3600). Only with `cronSchedule`. */
  cronTimeoutSecs?: number;
  memoryMb?: number;
  cpuMillis?: number;
  /** Each replica runs on a different server. */
  replicas?: number;
  /** Static sites only (repo or local source): absent = served from your servers. */
  hosting?: CloudflareHosting;
}
export interface CloudflareHosting {
  type: "cloudflare";
  /** Build output, relative to the root directory. */
  outputDir: string;
  /** Serve index.html for unknown paths. */
  spa: boolean;
  /** Worker name; unset = `<service>-<environment>`. */
  scriptName?: string;
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
export interface CronPreview {
  valid: boolean;
  error: string | null;
  description: string | null;
  /** Up to 3 upcoming run times, RFC3339 UTC. */
  upcoming: string[];
}
export type CronRunStatus = "queued" | "running" | "succeeded" | "failed" | "timed_out" | "skipped";
export interface CronRun {
  id: string;
  deploymentId: string | null;
  trigger: "schedule" | "manual";
  status: CronRunStatus;
  scheduledFor: string | null;
  timeoutSecs: number;
  createdAt: string;
  startedAt: string | null;
  finishedAt: string | null;
  exitCode: number | null;
  error: string | null;
}
export interface CronRuns {
  schedule: {
    expression: string;
    description: string;
    upcoming: string[];
    /** False while the schedule is only in settings and not deployed yet. */
    deployed: boolean;
    timeoutSecs: number;
  } | null;
  /** Newest first, at most 50. */
  runs: CronRun[];
}
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
  /** An R2 bucket service's bucket in this environment (null = none yet). */
  r2?: R2Link | null;
  updatedAt: string;
}
export interface R2Link {
  bucket: string;
  accountId: string;
  /** Created by Modula (only then can it be deleted from here). */
  created: boolean;
  createdAt: string;
}
/** A new R2 bucket by name, or an existing one to link. */
export type BucketChoice = { bucket: string } | { existing: string };
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
export type TemplateId = "postgres" | "mysql" | "redis" | "mongo" | "bucket";
export interface Template {
  id: TemplateId;
  name: string;
  description: string;
  image: string;
  defaultName: string;
  /** Variables a client connects with, e.g. DATABASE_URL. */
  connect: { label: string; variable: string }[];
}
/** A one-click app from the gallery: the app and the databases it needs. */
export interface AppTemplate {
  id: string;
  name: string;
  description: string;
  category: string;
  docsUrl: string;
  /** In creation order (databases first). */
  services: AppTemplateService[];
}
export interface AppTemplateService {
  /** Default service name. */
  role: string;
  /** A database template, for databases. */
  template: TemplateId | null;
  image: string;
  mountPath: string | null;
  /** Gets a generated domain (the web service). */
  domain: boolean;
}
export interface CreatedApp {
  template: string;
  services: { id: string; name: string; role: string; instanceId: string; web: boolean }[];
  domain: string | null;
  warnings: string[];
}
export type BackupProvider = "s3" | "r2" | "b2" | "minio";
export type BackupTargetKind = "s3" | "bucket" | "local";
export interface BackupTarget {
  id: string;
  name: string;
  kind: BackupTargetKind;
  /** Null for local targets. */
  provider: BackupProvider | null;
  /** Empty for bucket and local targets. */
  endpoint: string;
  region: string;
  bucket: string;
  prefix: string;
  accessKeyId: string;
  /** Bucket targets: the bucket service instance and where it lives. */
  bucketInstanceId: string | null;
  serviceName: string | null;
  environmentName: string | null;
  projectName: string | null;
  /** Local targets: the server whose disk holds the backups. */
  nodeId: string | null;
  nodeName: string | null;
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
export interface BackupRetention {
  dailyDays: number;
  weeklyDays: number;
  monthlyDays: number;
}
/** Exactly one of these selects where a backup is written. */
export type BackupDestinationChoice =
  | { targetId: string }
  | { bucketInstanceId: string }
  | { localDisk: true };
export interface BackupSchedule {
  enabled: boolean;
  targetId: string;
  targetName: string;
  targetKind: BackupTargetKind;
  hourUtc: number;
  retention: BackupRetention;
}
export type BackupScheduleInput = {
  enabled: boolean;
  hourUtc: number;
  retention?: BackupRetention;
} & BackupDestinationChoice;
export interface BackupBucketOption {
  instanceId: string;
  template: "r2" | "bucket";
  serviceName: string;
  environmentName: string;
  projectName: string;
  targetId: string | null;
}
export interface BackupServerOption {
  nodeId: string;
  name: string;
  targetId: string | null;
}
export interface BackupDestinations {
  targets: BackupTarget[];
  buckets: BackupBucketOption[];
  servers: BackupServerOption[];
}
export type BackupStatus =
  | "running"
  | "completed"
  | "failed"
  | "deleting"
  | "deleted";
export interface Backup {
  id: string;
  status: BackupStatus;
  kind: "scheduled" | "manual";
  sizeBytes: number | null;
  startedAt: string;
  finishedAt: string | null;
  error: string | null;
  targetName: string;
  targetKind: BackupTargetKind;
  /** `dump`, or `walg` for a base backup used by point-in-time recovery. */
  method: "dump" | "walg";
  walgName: string | null;
}
export interface PitrConfig {
  enabled: boolean;
  targetId: string;
  targetName: string;
  targetKind: BackupTargetKind;
  windowDays: number;
  hourUtc: number;
}
export interface PitrArchiving {
  lastArchivedAt: string | null;
  lastFailedAt: string | null;
  failedCount: number;
  inRecovery: boolean;
}
export interface PitrRestore {
  id: string;
  sourceInstanceId: string;
  instanceId: string;
  serviceName: string;
  targetTime: string;
  status: "running" | "completed" | "failed";
  error: string | null;
  startedAt: string;
  finishedAt: string | null;
}
export interface PitrInfo {
  config: PitrConfig | null;
  window: { from: string; to: string } | null;
  archiving: PitrArchiving | null;
  baseBackups: Backup[];
  restores: PitrRestore[];
}
export type PitrInput = {
  enabled: boolean;
  windowDays?: number;
  hourUtc?: number;
} & Partial<BackupDestinationChoice>;
export interface PitrRestoreInput {
  targetTime: string;
  into: "new" | "same";
  name?: string;
  confirm?: string;
}
export interface PitrRestored {
  restoreId: string;
  instanceId: string;
  serviceId: string;
  serviceName: string;
}
export interface ReplicationPeer {
  serviceId: string;
  serviceName: string;
  instanceId: string | null;
  nodeId: string | null;
}
export interface CreatedReplica {
  serviceId: string;
  serviceName: string;
  instanceId: string;
  nodeId: string;
}
export interface Standby {
  name: string;
  address: string | null;
  state: string | null;
  replayLagSeconds: number | null;
  lagBytes: number | null;
}
export type ReplicationStatus =
  | { role: "primary"; standbys: Standby[] }
  | { role: "replica"; replayLagSeconds: number | null; caughtUp: boolean; receiver: string | null };
export interface Replication {
  role: "primary" | "replica" | "standalone";
  primary: ReplicationPeer | null;
  replicas: ReplicationPeer[];
  status: ReplicationStatus | null;
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
export type UpdateMode = "off" | "notify" | "auto";
export interface UpdateWindow {
  days: number[]; // 0=Sunday … 6=Saturday
  time: string; // "HH:MM" 24h
  timezone: string; // IANA
}
export interface UpdateRun {
  id: string;
  fromVersion: string;
  toVersion: string;
  status: "requested" | "running" | "succeeded" | "failed" | "rolled_back";
  trigger: "auto" | "manual";
  requestedAt: string;
  finishedAt: string | null;
  log: string | null;
}
export interface UpdateServer {
  nodeId: string;
  name: string;
  version: string | null;
  status: "current" | "upgrading" | "failed" | "manual" | "offline" | "behind";
  error: string | null;
}
/** Private dashboard (Cloudflare Tunnel + Access), instance-wide. */
export type PrivateAccessPhase = "off" | "enabling" | "on" | "disabling";
export interface PrivateAccessStep {
  label: string;
  status: "running" | "done" | "failed";
  detail?: string;
}
export interface PrivateAccessStatus {
  phase: PrivateAccessPhase;
  dashboardHost: string | null;
  nodeHost: string | null;
  workspaceId: string | null;
  teamDomain: string | null;
  allowedEmails: string[];
  allowedDomains: string[];
  steps: PrivateAccessStep[];
  error: string | null;
  breakGlass: string;
}
export interface PrivateAccessCheck {
  items: { label: string; ok: boolean; detail: string | null }[];
  suggestedEmails: string[];
  ready: boolean;
}
export interface PrivateAccessUpdate {
  enabled: boolean;
  workspaceId?: string;
  nodeHost?: string;
  allowedEmails?: string[];
  allowedDomains?: string[];
}

export interface UpdateStatus {
  currentVersion: string;
  latestVersion: string | null;
  updateAvailable: boolean;
  releaseNotesUrl: string | null;
  checkedAt: string | null;
  checkError: string | null;
  mode: UpdateMode;
  channel: "stable";
  window: UpdateWindow;
  nextWindow: string | null;
  canUpgrade: boolean;
  lastRun: UpdateRun | null;
  servers: UpdateServer[];
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
  services: {
    serviceId: string;
    serviceName: string;
    names: string[];
    platform: string[];
  }[];
}
/** A variable or setting that differs from the service's last deployment. */
export interface PendingChange extends ActivityChange {
  /** Deployed and new values: only when asked for (Members and up), never for sealed variables. */
  currentValue?: string | null;
  newValue?: string | null;
  sealed?: boolean;
  /** A shared variable the service references. */
  shared?: boolean;
  /** Can be reverted on its own (POST …/discard). */
  discardable?: boolean;
}
export interface PendingService {
  serviceId: string;
  serviceName: string;
  instanceId: string;
  changes: PendingChange[];
}
export interface PendingChanges {
  count: number;
  services: PendingService[];
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
  /** What the person deploying wrote when applying changes. */
  message?: string | null;
  commitAuthor: string | null;
  /** What was deployed: `owner/name@branch`, the image, or "Uploaded via CLI". */
  source: string;
  buildStartedAt: string | null;
  buildFinishedAt: string | null;
  sourceKind?: "github" | "image" | "upload";
  /** Uploads (`modula up`): the local branch, whether uncommitted changes were included,
   * and whether the files are still kept (so it can be redeployed or rolled back to). */
  sourceBranch?: string | null;
  sourceDirty?: boolean | null;
  uploadAvailable?: boolean | null;
  /** manual | push | rollback | redeploy | restart | upload */
  trigger: string;
  /** "cloudflare": a static site served by a Cloudflare Worker at `url`. */
  hosting?: "cloudflare" | null;
  url?: string | null;
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
  info: {
    hostname?: string;
    os?: string;
    arch?: string;
    cpus?: number;
    memoryBytes?: number;
  };
  desiredGeneration: number;
  appliedGeneration: number;
  lastSeenAt: string | null;
  certExpiresAt: string;
  createdAt: string;
  connected: boolean;
  location?: Location | null;
}
/** The private network between servers (WireGuard mesh). */
export interface MeshNetwork {
  nodes: {
    id: string;
    name: string;
    /** This server's /24 of 10.80.0.0/16; null = not in the network. */
    subnet: string | null;
    connected: boolean;
    /** Unix seconds of its last tunnel report. */
    reportedAt: number | null;
  }[];
  /** One per ordered pair: how `from` reaches `to` and what it measured. */
  links: {
    from: string;
    to: string;
    path: "private" | "public" | "none";
    endpoint: string | null;
    latencyMs: number | null;
    lastHandshake: number | null;
  }[];
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
  connections: {
    used: number;
    max: number;
    active: number;
    idle: number;
    idleInTransaction: number;
  };
  cacheHitRatio: number;
  sizes: {
    database: number;
    tables: number;
    indexes: number;
    system: number;
    other: number;
    wal: number;
  };
  tables: DbTableStats[];
  unusedIndexes: {
    name: string;
    table: string;
    bytes: number;
    scans: number;
  }[];
  pgStatStatements: boolean;
  topQueries: {
    query: string;
    calls: number;
    meanMs: number;
    totalMs: number;
  }[];
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

export type MetricsRange = "1h" | "6h" | "24h" | "7d";
export interface MetricsPoint {
  /** Unix ms. */
  t: number;
  /** Percent of ONE vCPU (100 = one core; may exceed 100). */
  cpuPercent: number;
  memoryBytes: number;
  netRxBytesPerSec: number;
  netTxBytesPerSec: number;
}
export interface InstanceMetrics {
  range: MetricsRange;
  stepSecs: number;
  cpus: number;
  memoryTotalBytes: number;
  memoryLimitBytes: number | null;
  points: MetricsPoint[];
}

export interface LogLine {
  /** Unix ms. */
  t: number;
  serviceId: string;
  serviceName: string;
  deploymentId: string;
  stream: "stdout" | "stderr";
  line: string;
}
export interface LogBucket {
  /** Bucket start, unix ms. */
  t: number;
  count: number;
  errors: number;
}
export interface EnvironmentLogs {
  /** Oldest first; when truncated, the newest `limit` lines. */
  lines: LogLine[];
  histogram: LogBucket[];
  bucketMs: number;
  truncated: boolean;
}
export interface EnvironmentLogsQuery {
  since: number;
  until?: number;
  q?: string;
  service?: string;
  limit?: number;
}

export type WebhookEvent =
  "deployment.succeeded" | "deployment.failed" | "deployment.crashed";
export interface Webhook {
  id: string;
  url: string;
  events: WebhookEvent[];
  createdAt: string;
  lastStatus: number | null;
  lastError: string | null;
  lastAt: string | null;
}

export interface InstanceDomain {
  installUrl: string;
  serverIp: string | null;
  dashboard: string | null;
  appsDomain: string | null;
  appsDomainFromEnv: boolean;
}

const enc = encodeURIComponent;

export const api = {
  instance: () => get<InstanceStatus>("/api/instance"),
  me: () => get<Me>("/api/auth/me"),
  setup: (b: {
    setupToken: string;
    email: string;
    name: string;
    password: string;
    workspaceName?: string;
  }) => post<Me>("/api/auth/setup", b),
  login: (b: { email: string; password: string }) =>
    post<LoginResponse>("/api/auth/login", b),
  mfa: (b: { totpCode: string } | { backupCode: string }) =>
    post<Me>("/api/auth/mfa", b),
  sudo: (
    b: { password: string } | { totpCode: string } | { backupCode: string },
  ) => post<{ sudoUntil: string }>("/api/auth/sudo", b),
  logout: () => post<{ ok: true }>("/api/auth/logout"),
  security: () => get<Security>("/api/account/security"),
  totpBegin: () =>
    post<{ otpauthUrl: string; secret: string }>("/api/account/totp"),
  totpConfirm: (code: string) =>
    post<{ backupCodes: string[] }>("/api/account/totp/confirm", { code }),
  totpDisable: () => del<{ ok: true }>("/api/account/totp"),
  backupCodes: () =>
    post<{ backupCodes: string[] }>("/api/account/backup-codes"),
  /** Signs out other sessions and, unless `keepTokens`, revokes every API token. */
  changePassword: (newPassword: string, keepTokens = false) =>
    post<{ ok: true; sessionsRevoked: number; tokensRevoked: number }>("/api/account/password", {
      newPassword,
      keepTokens,
    }),
  sessions: () => get<SessionInfo[]>("/api/account/sessions"),
  revokeSession: (id: string) =>
    del<{ ok: true }>(`/api/account/sessions/${encodeURIComponent(id)}`),
  apiTokens: () => get<ApiToken[]>("/api/account/tokens"),
  /** Needs sudo (the request layer asks). The token is returned only here. */
  createApiToken: (b: {
    name: string;
    workspaceId?: string;
    expiresInDays?: number;
  }) => post<ApiToken & { token: string }>("/api/account/tokens", b),
  revokeApiToken: (id: string) =>
    del<{ ok: true }>(`/api/account/tokens/${encodeURIComponent(id)}`),
  createWorkspace: (name: string) =>
    post<{ id: string; name: string }>("/api/workspaces", { name }),
  updateWorkspace: (id: string, name: string) =>
    patch<{ id: string; name: string }>(`/api/workspaces/${enc(id)}`, { name }),
  deleteWorkspace: (id: string) =>
    del<{ ok: true }>(`/api/workspaces/${enc(id)}`),

  members: (workspace: string) =>
    get<Member[]>(`/api/workspaces/${enc(workspace)}/members`),
  /** Needs sudo (the request layer asks). */
  updateMember: (id: string, role: Role) =>
    patch<Member>(`/api/members/${enc(id)}`, { role }),
  /** Needs sudo (the request layer asks). */
  removeMember: (id: string) => del<{ ok: true }>(`/api/members/${enc(id)}`),
  leaveWorkspace: (workspace: string) =>
    post<{ ok: true }>(`/api/workspaces/${enc(workspace)}/leave`),
  invites: (workspace: string) =>
    get<Invite[]>(`/api/workspaces/${enc(workspace)}/invites`),
  /** The link is returned only here. */
  createInvite: (workspace: string, b: { role: Role; expiresInDays?: number }) =>
    post<{ invite: Invite; url: string }>(`/api/workspaces/${enc(workspace)}/invites`, b),
  revokeInvite: (id: string) => del<{ ok: true }>(`/api/invites/${enc(id)}`),
  lookupInvite: (token: string) =>
    post<InvitePreview>("/api/invites/lookup", { token }),
  acceptInvite: (token: string) =>
    post<InviteJoined>("/api/invites/accept", { token }),
  /** Creates the account, joins the workspace and signs in (session cookie). */
  signupWithInvite: (b: { token: string; name: string; email: string; password: string }) =>
    post<InviteJoined>("/api/invites/signup", b),

  projects: (workspace: string) =>
    get<ProjectCard[]>(`/api/workspaces/${enc(workspace)}/projects`),
  createProject: (
    workspace: string,
    b: { name: string; description?: string },
  ) => post<ProjectDetail>(`/api/workspaces/${enc(workspace)}/projects`, b),
  project: (id: string) => get<ProjectDetail>(`/api/projects/${enc(id)}`),
  updateProject: (id: string, b: { name?: string; description?: string }) =>
    patch<Project>(`/api/projects/${enc(id)}`, b),
  deleteProject: (id: string) => del<{ ok: true }>(`/api/projects/${enc(id)}`),
  /** Without `duplicateFrom`, an empty environment (default environment's
   *  settings, no variables). `branches`: service id → branch, repo services only. */
  createEnvironment: (
    project: string,
    b: { name: string; duplicateFrom?: string; branches?: Record<string, string> },
  ) => post<Environment>(`/api/projects/${enc(project)}/environments`, b),
  renameEnvironment: (id: string, name: string) =>
    patch<Environment>(`/api/environments/${enc(id)}`, { name }),
  syncEnvironment: (target: string, fromEnvironmentId: string) =>
    post<SyncResult>(`/api/environments/${enc(target)}/sync`, {
      fromEnvironmentId,
    }),
  activity: (
    project: string,
    q: { environmentId?: string; serviceId?: string; kind?: ActivityKind },
  ) => {
    const params = new URLSearchParams();
    if (q.environmentId) params.set("environmentId", q.environmentId);
    if (q.serviceId) params.set("serviceId", q.serviceId);
    if (q.kind) params.set("kind", q.kind);
    return get<{ items: ActivityGroup[] }>(
      `/api/projects/${enc(project)}/activity?${params}`,
    );
  },
  deleteEnvironment: (id: string) =>
    del<{ ok: true }>(`/api/environments/${enc(id)}`),

  createService: (
    project: string,
    b:
      | {
          name: string;
          config: Partial<ServiceConfig> & { source: ServiceConfig["source"] };
        }
      | { name: string; template: TemplateId },
  ) =>
    post<{ id: string; name: string; instances: Instance[] }>(
      `/api/projects/${enc(project)}/services`,
      b,
    ),
  /** `warnings`: R2 bucket tokens left at Cloudflare (it was disconnected). */
  deleteService: (id: string) => del<{ ok: true; warnings?: string[] }>(`/api/services/${enc(id)}`),
  createBucket: (project: string, b: { name: string; environmentId: string } & BucketChoice) =>
    post<{ id: string; name: string; instances: Instance[] }>(`/api/projects/${enc(project)}/buckets`, b),
  provisionBucket: (instance: string, b: BucketChoice) => post<Instance>(`/api/service-instances/${enc(instance)}/r2`, b),
  unlinkBucket: (instance: string) => del<{ ok: true; warnings?: string[] }>(`/api/service-instances/${enc(instance)}/r2`),
  deleteBucket: (instance: string, confirm: string) =>
    post<{ ok: true }>(`/api/service-instances/${enc(instance)}/r2/delete-bucket`, { confirm }),
  serviceInstance: (id: string) =>
    get<Instance>(`/api/service-instances/${enc(id)}`),
  moveInstance: (id: string, nodeId: string) =>
    patch<{ nodeId: string; deploymentId: string | null }>(
      `/api/service-instances/${enc(id)}/server`,
      { nodeId },
    ),
  cronPreview: (schedule: string) =>
    get<CronPreview>(`/api/cron/preview?schedule=${encodeURIComponent(schedule)}`),
  cronRuns: (instance: string) =>
    get<CronRuns>(`/api/service-instances/${enc(instance)}/cron-runs`),
  runCronNow: (instance: string) =>
    post<CronRun>(`/api/service-instances/${enc(instance)}/cron-runs`),
  updateInstance: (id: string, config: ServiceConfig) =>
    patch<Instance>(`/api/service-instances/${enc(id)}`, { config }),

  variables: (instance: string) =>
    get<VariableInfo[]>(`/api/service-instances/${enc(instance)}/variables`),
  updateVariables: (instance: string, b: VariablePatch) =>
    patch<VariableInfo[]>(
      `/api/service-instances/${enc(instance)}/variables`,
      b,
    ),
  sharedVariables: (environment: string) =>
    get<VariableInfo[]>(`/api/environments/${enc(environment)}/variables`),
  updateSharedVariables: (environment: string, b: VariablePatch) =>
    patch<VariableInfo[]>(`/api/environments/${enc(environment)}/variables`, b),
  variableNames: (environment: string) =>
    get<VariableNames>(`/api/environments/${enc(environment)}/variable-names`),
  pending: (environment: string, opts?: { values?: boolean }) =>
    get<PendingChanges>(
      `/api/environments/${enc(environment)}/pending${opts?.values ? "?values=true" : ""}`,
    ),
  deployPending: (
    environment: string,
    b: { serviceIds?: string[]; message?: string } = {},
  ) =>
    post<{
      deployments: {
        serviceId: string;
        deploymentId: string | null;
        error: string | null;
      }[];
    }>(`/api/environments/${enc(environment)}/deploy`, b),
  /** Revert a service's pending changes (all, or the given ones) to its last deployment. */
  discardPending: (
    environment: string,
    serviceId: string,
    changes?: { kind: "variable" | "config"; key: string }[],
  ) =>
    post<PendingChanges>(`/api/environments/${enc(environment)}/discard`, {
      serviceId,
      changes,
    }),
  revealVariable: (id: string) =>
    post<{ name: string; value: string }>(`/api/variables/${enc(id)}/reveal`),

  deployments: (instance: string) =>
    get<Deployment[]>(`/api/service-instances/${enc(instance)}/deployments`),
  instanceMetrics: (instance: string, range: MetricsRange) =>
    get<InstanceMetrics>(
      `/api/service-instances/${enc(instance)}/metrics?range=${range}`,
    ),
  projectMetrics: (project: string, environment: string, range: MetricsRange) =>
    get<InstanceMetrics>(
      `/api/projects/${enc(project)}/metrics?environment=${enc(environment)}&range=${range}`,
    ),
  environmentLogs: (environment: string, q: EnvironmentLogsQuery) => {
    const params = new URLSearchParams({ since: String(q.since) });
    if (q.until !== undefined) params.set("until", String(q.until));
    if (q.q) params.set("q", q.q);
    if (q.service) params.set("service", q.service);
    if (q.limit !== undefined) params.set("limit", String(q.limit));
    return get<EnvironmentLogs>(
      `/api/environments/${enc(environment)}/logs?${params}`,
    );
  },
  webhooks: (project: string) =>
    get<Webhook[]>(`/api/projects/${enc(project)}/webhooks`),
  /** The signing secret is returned only here. */
  createWebhook: (
    project: string,
    b: { url: string; events: WebhookEvent[] },
  ) =>
    post<Webhook & { secret: string }>(
      `/api/projects/${enc(project)}/webhooks`,
      b,
    ),
  deleteWebhook: (id: string) => del<void>(`/api/webhooks/${enc(id)}`),
  testWebhook: (id: string) =>
    post<{ status: number | null; error: string | null }>(
      `/api/webhooks/${enc(id)}/test`,
    ),
  deploy: (instance: string) =>
    post<DeploymentDetail>(
      `/api/service-instances/${enc(instance)}/deployments`,
    ),
  deployment: (id: string) =>
    get<DeploymentDetail>(`/api/deployments/${enc(id)}`),
  redeployDeployment: (id: string) =>
    post<DeploymentDetail>(`/api/deployments/${enc(id)}/redeploy`),
  rollbackDeployment: (id: string) =>
    post<DeploymentDetail>(`/api/deployments/${enc(id)}/rollback`),
  restartDeployment: (id: string) =>
    post<DeploymentDetail>(`/api/deployments/${enc(id)}/restart`),
  removeDeployment: (id: string) =>
    del<DeploymentDetail>(`/api/deployments/${enc(id)}`),
  deploymentLogsUrl: (
    id: string,
    tail = 500,
    kind: "deploy" | "build" = "deploy",
  ) =>
    `/api/deployments/${enc(id)}/logs?${kind === "build" ? "kind=build&" : ""}tail=${tail}&follow=true`,

  domains: (instance: string) =>
    get<Domain[]>(`/api/service-instances/${enc(instance)}/domains`),
  generateDomain: (instance: string) =>
    post<Domain>(`/api/service-instances/${enc(instance)}/domains`),
  addCustomDomain: (
    instance: string,
    body: { hostname: string; targetPort: number | null },
  ) =>
    post<Domain>(
      `/api/service-instances/${enc(instance)}/custom-domains`,
      body,
    ),
  checkDomain: (id: string) => post<Domain>(`/api/domains/${enc(id)}/check`),
  updateDomain: (
    id: string,
    body: { targetPort: number | null; rateLimitPerMinute: number | null },
  ) => patch<Domain>(`/api/domains/${enc(id)}`, body),
  deleteDomain: (id: string) => del<{ ok: true }>(`/api/domains/${enc(id)}`),

  templates: () => get<Template[]>("/api/templates"),
  appTemplates: () => get<AppTemplate[]>("/api/app-templates"),
  createApp: (project: string, b: { template: string; environmentId: string; namePrefix?: string }) =>
    post<CreatedApp>(`/api/projects/${enc(project)}/app-templates`, b),
  createVolume: (instance: string, mountPath: string) =>
    post<Instance>(`/api/service-instances/${enc(instance)}/volume`, {
      mountPath,
    }),
  deleteVolume: (id: string) => del<{ ok: true }>(`/api/volumes/${enc(id)}`),
  createTcpProxy: (instance: string, targetPort: number) =>
    post<Instance>(`/api/service-instances/${enc(instance)}/tcp-proxy`, {
      targetPort,
    }),
  deleteTcpProxy: (id: string) =>
    del<{ ok: true }>(`/api/tcp-proxies/${enc(id)}`),

  backupTargets: (workspace: string) =>
    get<BackupTarget[]>(`/api/workspaces/${enc(workspace)}/backup-targets`),
  createBackupTarget: (workspace: string, b: BackupTargetInput) =>
    post<BackupTarget>(`/api/workspaces/${enc(workspace)}/backup-targets`, b),
  deleteBackupTarget: (id: string) =>
    del<{ ok: true }>(`/api/backup-targets/${enc(id)}`),
  backups: (instance: string) =>
    get<BackupsInfo>(`/api/service-instances/${enc(instance)}/backups`),
  backupDestinations: (instance: string) =>
    get<BackupDestinations>(
      `/api/service-instances/${enc(instance)}/backup-destinations`,
    ),
  saveBackupSchedule: (instance: string, b: BackupScheduleInput) =>
    request<BackupsInfo>(
      "PUT",
      `/api/service-instances/${enc(instance)}/backup-schedule`,
      b,
    ),
  backupNow: (instance: string, dest?: BackupDestinationChoice) =>
    post<Backup>(`/api/service-instances/${enc(instance)}/backups`, dest),
  pitr: (instance: string) =>
    get<PitrInfo>(`/api/service-instances/${enc(instance)}/pitr`),
  savePitr: (instance: string, b: PitrInput) =>
    request<PitrInfo>("PUT", `/api/service-instances/${enc(instance)}/pitr`, b),
  pitrBaseBackup: (instance: string) =>
    post<PitrInfo>(`/api/service-instances/${enc(instance)}/pitr/base-backups`),
  pitrRestore: (instance: string, b: PitrRestoreInput) =>
    post<PitrRestored>(`/api/service-instances/${enc(instance)}/pitr/restore`, b),
  replication: (instance: string) =>
    get<Replication>(`/api/service-instances/${enc(instance)}/replication`),
  addReplica: (instance: string, b: { name?: string }) =>
    post<CreatedReplica>(`/api/service-instances/${enc(instance)}/replicas`, b),
  promoteReplica: (instance: string, confirm: string) =>
    post<Replication>(`/api/service-instances/${enc(instance)}/promote`, { confirm }),
  restoreBackup: (id: string) =>
    post<{ ok: true }>(`/api/backups/${enc(id)}/restore`),
  deleteBackup: (id: string) => del<Backup>(`/api/backups/${enc(id)}`),
  selfBackup: () => get<SelfBackup>("/api/instance/self-backup"),
  saveSelfBackup: (b: SelfBackup) =>
    request<SelfBackup>("PUT", "/api/instance/self-backup", b),
  updates: () => get<UpdateStatus>("/api/instance/updates"),
  saveUpdates: (b: {
    mode: UpdateMode;
    channel: "stable";
    window: UpdateWindow;
  }) => request<UpdateStatus>("PUT", "/api/instance/updates", b),
  runUpdate: () =>
    request<UpdateStatus>("POST", "/api/instance/updates/run", {}),
  checkUpdates: () =>
    request<UpdateStatus>("POST", "/api/instance/updates/check", {}),
  instanceDomain: () => get<InstanceDomain>("/api/instance/domain"),
  saveInstanceDomain: (b: {
    dashboard: string | null;
    appsDomain: string | null;
    skipDnsCheck?: boolean;
  }) => request<InstanceDomain>("PUT", "/api/instance/domain", b),
  privateAccess: () => get<PrivateAccessStatus>("/api/instance/private-access"),
  privateAccessCheck: (workspaceId: string) =>
    get<PrivateAccessCheck>(
      `/api/instance/private-access/check?workspaceId=${encodeURIComponent(workspaceId)}`,
    ),
  setPrivateAccess: (b: PrivateAccessUpdate) =>
    request<PrivateAccessStatus>("PUT", "/api/instance/private-access", b),

  dbTables: (i: string) =>
    get<{ tables: DbTable[] }>(
      `/api/service-instances/${enc(i)}/database/tables`,
    ),
  dbRows: (i: string, q: DbRowsQuery) => {
    const p = new URLSearchParams({
      table: q.table,
      page: String(q.page),
      pageSize: String(q.pageSize),
    });
    if (q.orderBy) {
      p.set("orderBy", q.orderBy);
      p.set("dir", q.dir ?? "asc");
    }
    return get<DbRows>(`/api/service-instances/${enc(i)}/database/rows?${p}`);
  },
  dbQuery: (i: string, sql: string) =>
    post<DbQueryResult>(`/api/service-instances/${enc(i)}/database/query`, {
      sql,
    }),
  dbInsertRow: (
    i: string,
    table: string,
    values: Record<string, string | null>,
  ) =>
    post<unknown>(`/api/service-instances/${enc(i)}/database/rows`, {
      table,
      values,
    }),
  dbUpdateRow: (
    i: string,
    table: string,
    key: Record<string, string | null>,
    values: Record<string, string | null>,
  ) =>
    patch<unknown>(`/api/service-instances/${enc(i)}/database/rows`, {
      table,
      key,
      values,
    }),
  dbDeleteRow: (i: string, table: string, key: Record<string, string | null>) =>
    request<unknown>(
      "DELETE",
      `/api/service-instances/${enc(i)}/database/rows`,
      { table, key },
    ),
  dbStats: (i: string) =>
    get<DbStats>(`/api/service-instances/${enc(i)}/database/stats`),
  dbVacuum: (i: string, table: string) =>
    post<unknown>(`/api/service-instances/${enc(i)}/database/vacuum`, {
      table,
    }),
  dbExtensions: (i: string) =>
    get<DbExtensions>(`/api/service-instances/${enc(i)}/database/extensions`),
  dbSetExtension: (i: string, name: string, action: "install" | "uninstall") =>
    post<unknown>(`/api/service-instances/${enc(i)}/database/extensions`, {
      name,
      action,
    }),
  dbCredentials: (i: string) =>
    get<DbCredentials>(`/api/service-instances/${enc(i)}/database/credentials`),
  dbRegeneratePassword: (i: string) =>
    post<DbCredentials>(`/api/service-instances/${enc(i)}/database/password`),

  nodes: () => get<NodeInfo[]>("/api/nodes"),
  meshNetwork: () => get<MeshNetwork>("/api/nodes/network"),
  createJoinToken: () => post<JoinToken>("/api/nodes/join-tokens"),
  locations: () => get<LocationOption[]>("/api/locations"),
  setNodeLocation: (id: string, location: string | null) =>
    patch<NodeInfo>(`/api/nodes/${enc(id)}`, { location }),
  revokeNode: (id: string) => del<{ ok: true }>(`/api/nodes/${enc(id)}`),
};
