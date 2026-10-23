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
}
export type DeploymentStatus = "queued" | "deploying" | "active" | "failed" | "crashed" | "removed" | "cancelled";
export interface InstanceSummary {
  id: string;
  serviceId: string;
  environmentId: string;
  nodeId: string | null;
  latestDeploymentId: string | null;
  latestDeploymentStatus: DeploymentStatus | null;
  domain: string | null;
}
export interface ProjectDetail extends Project {
  environments: Environment[];
  services: ServiceSummary[];
  instances: InstanceSummary[];
}
export type RestartPolicy = "on_failure" | "always" | "never";
export interface ServiceConfig {
  source: { type: "image"; image: string };
  startCommand?: string[];
  port: number;
  healthcheckPath?: string;
  healthcheckTimeoutSecs: number;
  restartPolicy: RestartPolicy;
  memoryMb?: number;
  cpuMillis?: number;
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
  updatedAt: string;
}
export interface VariableInfo {
  id: string;
  name: string;
  sealed: boolean;
  updatedAt: string;
}
export interface Deployment {
  id: string;
  serviceInstanceId: string;
  nodeId: string | null;
  status: DeploymentStatus;
  image: string;
  trigger: string;
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
export interface Domain {
  id: string;
  hostname: string;
  kind: "generated" | "custom";
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
}
export interface JoinToken {
  token: string;
  expiresAt: string;
  server: string;
  fingerprint: string;
  command: string;
}

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
  createWorkspace: (name: string) => post<{ id: string; name: string }>("/api/workspaces", { name }),

  projects: (workspace: string) => get<Project[]>(`/api/workspaces/${enc(workspace)}/projects`),
  createProject: (workspace: string, b: { name: string; description?: string }) =>
    post<ProjectDetail>(`/api/workspaces/${enc(workspace)}/projects`, b),
  project: (id: string) => get<ProjectDetail>(`/api/projects/${enc(id)}`),
  deleteProject: (id: string) => del<{ ok: true }>(`/api/projects/${enc(id)}`),
  createEnvironment: (project: string, name: string) =>
    post<Environment>(`/api/projects/${enc(project)}/environments`, { name }),
  deleteEnvironment: (id: string) => del<{ ok: true }>(`/api/environments/${enc(id)}`),

  createService: (project: string, b: { name: string; config: Partial<ServiceConfig> & { source: ServiceConfig["source"] } }) =>
    post<{ id: string; name: string; instances: Instance[] }>(`/api/projects/${enc(project)}/services`, b),
  deleteService: (id: string) => del<{ ok: true }>(`/api/services/${enc(id)}`),
  serviceInstance: (id: string) => get<Instance>(`/api/service-instances/${enc(id)}`),
  updateInstance: (id: string, config: ServiceConfig) => patch<Instance>(`/api/service-instances/${enc(id)}`, { config }),

  variables: (instance: string) => get<VariableInfo[]>(`/api/service-instances/${enc(instance)}/variables`),
  updateVariables: (instance: string, b: { set?: Record<string, string>; sealed?: string[]; delete?: string[] }) =>
    patch<VariableInfo[]>(`/api/service-instances/${enc(instance)}/variables`, b),
  revealVariable: (id: string) => post<{ name: string; value: string }>(`/api/variables/${enc(id)}/reveal`),

  deployments: (instance: string) => get<Deployment[]>(`/api/service-instances/${enc(instance)}/deployments`),
  deploy: (instance: string) => post<DeploymentDetail>(`/api/service-instances/${enc(instance)}/deployments`),
  deployment: (id: string) => get<DeploymentDetail>(`/api/deployments/${enc(id)}`),
  removeDeployment: (id: string) => del<DeploymentDetail>(`/api/deployments/${enc(id)}`),
  deploymentLogsUrl: (id: string, tail = 500) => `/api/deployments/${enc(id)}/logs?tail=${tail}&follow=true`,

  domains: (instance: string) => get<Domain[]>(`/api/service-instances/${enc(instance)}/domains`),
  generateDomain: (instance: string) => post<Domain>(`/api/service-instances/${enc(instance)}/domains`),
  deleteDomain: (id: string) => del<{ ok: true }>(`/api/domains/${enc(id)}`),

  nodes: () => get<NodeInfo[]>("/api/nodes"),
  createJoinToken: () => post<JoinToken>("/api/nodes/join-tokens"),
  revokeNode: (id: string) => del<{ ok: true }>(`/api/nodes/${enc(id)}`),
};
