import type { Role } from "./api";

const rank: Record<Role, number> = { viewer: 0, deployer: 1, member: 2, admin: 3 };

/** Mirrors `Action::min_role` on the server; the server still decides. */
export const atLeast = (role: Role | undefined, min: Role) => role !== undefined && rank[role] >= rank[min];
