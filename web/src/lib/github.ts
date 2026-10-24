// GitHub App integration (mirrors crates/server/src/api/github.rs). Nothing here
// ever receives a token: the server keeps installation tokens to itself.

import { get, post, type Role } from "./api";
import { atLeast } from "./roles";
import { safeHttpUrl } from "./url";

/** One GitHub App of the instance (one private App per GitHub account/organization). */
export interface GithubApp {
  id: number;
  slug: string;
  htmlUrl: string;
  /** The GitHub account or organization that owns it (the only one a private App installs on). */
  ownerLogin: string | null;
  public: boolean;
  createdAt: string;
  /** Permissions (`name: level`) and events the App lacks: its owner approves them on GitHub. */
  missingPermissions: string[];
  missingEvents: string[];
  /** Where the App's owner reviews its permissions and events. */
  permissionsUrl: string;
}
export interface GithubApps {
  apps: GithubApp[];
  /** Where GitHub delivers webhooks: this instance's public URL. */
  webhookUrl: string;
}
export interface GithubInstallation {
  id: number;
  accountLogin: string;
  accountType: "Organization" | "User" | string;
  avatarUrl: string;
  /** The installation's settings on GitHub (repository access). */
  configureUrl: string;
  createdAt: string;
  /** The App this installation belongs to. */
  appId: number;
  appSlug: string;
}
export interface GithubRepo {
  fullName: string;
  private: boolean;
  defaultBranch: string;
  /** Last push. */
  updatedAt: string | null;
  ownerAvatar: string | null;
}
export interface GithubBranch {
  name: string;
  protected: boolean;
}
export interface ManifestStart {
  /** POST a form with a `manifest` field here. */
  url: string;
  manifest: Record<string, unknown>;
  state: string;
}

const ws = (id: string) => `/api/workspaces/${id}/github`;
const qs = (q: string) =>
  q.trim() ? `?q=${encodeURIComponent(q.trim())}&perPage=100` : "?perPage=100";

export const github = {
  apps: () => get<GithubApps>("/api/github/apps"),
  /** Instance admins; sudo (the request layer asks). Removes its installations everywhere. */
  deleteApp: (id: number) => post<void>("/api/github/apps/remove", { id }),
  /** Instance admins (sudo; the request layer asks). With a workspace, GitHub goes on to
   *  installing the new App there and comes back to `returnTo`. */
  manifest: (
    o: { org?: string; workspaceId?: string; returnTo?: string } = {},
  ) =>
    post<ManifestStart>("/api/github/app/manifest", {
      org: o.org?.trim() || undefined,
      workspaceId: o.workspaceId,
      returnTo: o.returnTo,
    }),
  /** `appId` is required once the instance has more than one App. */
  installUrl: (workspaceId: string, returnTo?: string, appId?: number) =>
    post<{ url: string }>(`${ws(workspaceId)}/install`, { returnTo, appId }),
  installations: (workspaceId: string) =>
    get<GithubInstallation[]>(`${ws(workspaceId)}/installations`),
  repos: (workspaceId: string, q = "") =>
    get<GithubRepo[]>(`${ws(workspaceId)}/repos${qs(q)}`),
  branches: (workspaceId: string, repo: string, q = "") =>
    get<GithubBranch[]>(
      `${ws(workspaceId)}/branches${qs(q)}&repo=${encodeURIComponent(repo)}`,
    ),
};

/** Send the browser to GitHub with the App manifest (GitHub only takes it as a form POST). */
export function submitManifest(start: ManifestStart) {
  const action = safeHttpUrl(start.url, true);
  if (!action) throw new Error("unexpected GitHub URL");
  const form = document.createElement("form");
  form.method = "post";
  form.action = action;
  const input = document.createElement("input");
  input.type = "hidden";
  input.name = "manifest";
  input.value = JSON.stringify(start.manifest);
  form.appendChild(input);
  document.body.appendChild(form);
  form.submit();
}

/** What "Connect GitHub" does for this user: start the flow, or who to ask. */
export type ConnectMode =
  "connect" | "ask-instance-admin" | "ask-workspace-admin";

export function connectMode(o: {
  appCount: number;
  isInstanceAdmin: boolean;
  role: Role | undefined;
}): ConnectMode {
  // Installing binds GitHub to the workspace: its admins only.
  if (!atLeast(o.role, "admin")) return "ask-workspace-admin";
  // Without an App, the flow starts by creating one: instance admins only.
  if (o.appCount === 0 && !o.isInstanceAdmin) return "ask-instance-admin";
  return "connect";
}

/** What "Connect GitHub" starts: create the first App, install the only App, or pick whose. */
export const connectStep = (
  appCount: number,
): "add-app" | "install" | "pick" =>
  appCount === 0 ? "add-app" : appCount === 1 ? "install" : "pick";

/** The App's owner as shown to people (falls back to the slug). */
export const appOwner = (a: Pick<GithubApp, "ownerLogin" | "slug">) =>
  a.ownerLogin || a.slug;

/** GitHub's avatar for an account login. */
export const githubAvatar = (login: string) =>
  `https://avatars.githubusercontent.com/${encodeURIComponent(login)}?s=64`;

export const connectHelp: Record<Exclude<ConnectMode, "connect">, string> = {
  "ask-instance-admin":
    "Ask an instance admin to connect GitHub. It's set up once for the whole instance.",
  "ask-workspace-admin": "Ask a workspace admin to connect GitHub.",
};

/** Install an existing App on the workspace (straight to GitHub's installation page), or, with
 *  no `appId`, create a new one first (`org`, else the admin's personal account; it then goes
 *  straight on to installing it). GitHub sends the browser back to `returnTo` with `?github=…`.
 *  Resolves once the browser is on its way. */
export async function connectGithub(o: {
  workspaceId: string;
  returnTo: string;
  appId?: number;
  org?: string;
}) {
  if (o.appId !== undefined) {
    const { url } = await github.installUrl(o.workspaceId, o.returnTo, o.appId);
    const safe = safeHttpUrl(url, true);
    if (!safe) throw new Error("unexpected GitHub URL");
    window.location.assign(safe);
  } else {
    submitManifest(
      await github.manifest({
        org: o.org,
        workspaceId: o.workspaceId,
        returnTo: o.returnTo,
      }),
    );
  }
}

/** Where "Connect GitHub" in the New Service dialog comes back to: the project, with the dialog
 *  reopened on the GitHub step. */
export function projectReturnPath(
  projectId: string,
  environmentId?: string,
): string {
  const q = new URLSearchParams();
  if (environmentId) q.set("env", environmentId);
  q.set("add", "repo");
  return `/projects/${encodeURIComponent(projectId)}?${q.toString()}`;
}

/** Where "Connect GitHub" in workspace settings comes back to. */
export const SETTINGS_RETURN_PATH = "/settings#integrations-github";

/** Back on a project from GitHub (`?add=repo&github=…`): whether to reopen the New Service
 *  dialog on the GitHub step, and what to say. */
export function projectReturn(search: { add?: string; github?: string }) {
  return {
    reopen: search.add === "repo",
    notice: search.github ? githubReturnNotice(search.github) : null,
  };
}

/** What `?github=` means after a round trip through GitHub. */
export function githubReturnNotice(
  param: string | null,
): { tone: "success" | "info"; text: string } | null {
  switch (param) {
    case "created":
      return {
        tone: "success",
        text: "GitHub App created. Install it on the account that owns it to pick repositories.",
      };
    case "installed":
      return { tone: "success", text: "GitHub connected — pick a repo." };
    case "requested":
      return {
        tone: "info",
        text: "Installation requested. Once an owner of the organization approves it on GitHub, click Connect GitHub again to finish.",
      };
    case "updated":
      return { tone: "info", text: "GitHub installation updated." };
    default:
      return null;
  }
}

/** Order picker items for what was typed: names starting with it first (for repositories,
 *  the part after `owner/`, then `owner/name`), then anywhere; within a group the server's order
 *  (most recently pushed) is kept. */
export function rankItems<T>(
  items: T[],
  query: string,
  key: (t: T) => string,
  ownerPrefixed = false,
): T[] {
  const q = query.trim().toLowerCase();
  // Empty, or exactly what was just picked: offer everything to switch to.
  if (!q || items.some((t) => key(t).toLowerCase() === q)) return items;
  const score = (t: T) => {
    const full = key(t).toLowerCase();
    if (ownerPrefixed && full.slice(full.indexOf("/") + 1).startsWith(q))
      return 0;
    if (full.startsWith(q)) return 1;
    if (full.includes(q)) return 2;
    return -1;
  };
  return items
    .map((t, i) => ({ t, i, s: score(t) }))
    .filter((x) => x.s >= 0)
    .sort((a, b) => a.s - b.s || a.i - b.i)
    .map((x) => x.t);
}

export const rankRepos = (repos: GithubRepo[], query: string) =>
  rankItems(repos, query, (r) => r.fullName, true);
export const rankBranches = (branches: GithubBranch[], query: string) =>
  rankItems(branches, query, (b) => b.name);

/** Next highlighted index for ↑/↓ in a list of `n` options (wraps; -1 when empty). */
export function moveHighlight(
  current: number,
  n: number,
  delta: 1 | -1,
): number {
  if (n <= 0) return -1;
  if (current < 0) return delta === 1 ? 0 : n - 1;
  return (current + delta + n) % n;
}

/** The account (login) a repository `owner/name` belongs to. */
export const repoOwner = (fullName: string) => fullName.split("/")[0] ?? "";

/** Repositories of one installed account (case-insensitive); everything when none is selected. */
export function filterReposByAccount(
  repos: GithubRepo[],
  login: string | null,
): GithubRepo[] {
  if (!login) return repos;
  const l = login.toLowerCase();
  return repos.filter((r) => repoOwner(r.fullName).toLowerCase() === l);
}

/** The account to show first: the one owning the most recently pushed repository (repos come
 *  newest first), else the first installation. */
export function defaultAccount(
  installations: GithubInstallation[],
  repos: GithubRepo[],
): string | null {
  const logins = new Map(
    installations.map((i) => [i.accountLogin.toLowerCase(), i.accountLogin]),
  );
  const recent = [...repos]
    .filter(
      (r) => r.updatedAt && logins.has(repoOwner(r.fullName).toLowerCase()),
    )
    .sort((a, b) => Date.parse(b.updatedAt!) - Date.parse(a.updatedAt!))[0];
  if (recent)
    return logins.get(repoOwner(recent.fullName).toLowerCase()) ?? null;
  return installations[0]?.accountLogin ?? null;
}

/** The selected account if it is still installed, else the default. */
export function effectiveAccount(
  selected: string | null,
  installations: GithubInstallation[],
  repos: GithubRepo[],
): string | null {
  const hit = installations.find(
    (i) => i.accountLogin.toLowerCase() === selected?.toLowerCase(),
  );
  return hit ? hit.accountLogin : defaultAccount(installations, repos);
}
