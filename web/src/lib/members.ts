import type { Member, Role } from "./api";

export const ROLES: Role[] = ["admin", "member", "deployer", "viewer"];

/** What each role can do, mirroring `Action::min_role` on the server. */
export const ROLE_HINTS: Record<Role, string> = {
  admin: "Everything, including members, integrations and deletions",
  member: "Create and configure projects, services and domains",
  deployer: "Deploy, roll back and edit variables",
  viewer: "Read-only access",
};

export const roleLabel = (r: Role) => r.charAt(0).toUpperCase() + r.slice(1);

/** The server refuses to remove, demote or let leave the workspace's only admin. */
export function isLastAdmin(members: Member[], member: Member): boolean {
  return member.role === "admin" && members.filter((m) => m.role === "admin").length === 1;
}

/** "in 6 days" / "in 3 hours" / "expired". */
export function expiresIn(iso: string, now = Date.now()): string {
  const s = Math.round((new Date(iso).getTime() - now) / 1000);
  if (s <= 0) return "expired";
  const h = Math.floor(s / 3600);
  if (h < 1) return `in ${Math.max(1, Math.floor(s / 60))} min`;
  if (h < 48) return `in ${h} hour${h === 1 ? "" : "s"}`;
  return `in ${Math.floor(h / 24)} days`;
}

/** An invite token from a URL (`/login?invite=…`, `/invite/$token`), or undefined
 *  for anything that isn't token-shaped. Sign-in returns to `/invite/<token>` only. */
export function inviteToken(raw: unknown): string | undefined {
  if (typeof raw !== "string") return undefined;
  return /^[A-Za-z0-9_-]{1,128}$/.test(raw) ? raw : undefined;
}
