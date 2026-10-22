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
};
