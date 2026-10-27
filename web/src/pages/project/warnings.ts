import type { EnvWarning } from "@/lib/api";
import type { ServiceTab } from "./ServiceSheet";

/** Where clicking a warning goes: a service tab on this page (optionally focusing a control), or another page. */
export type WarningTarget =
  | { kind: "service"; serviceId: string; tab: ServiceTab; deploymentId?: string; focus?: { instanceId: string; target: "server" } }
  | { kind: "server"; nodeId: string }
  | { kind: "servers" }
  | { kind: "sharedVariables" };

export function warningTarget(w: EnvWarning): WarningTarget | null {
  const a = w.action;
  switch (a.type) {
    case "openDeployment":
      return w.serviceId ? { kind: "service", serviceId: w.serviceId, tab: "deployments", deploymentId: a.deploymentId } : null;
    case "moveServer":
      return w.serviceId && w.serviceInstanceId
        ? { kind: "service", serviceId: w.serviceId, tab: "settings", focus: { instanceId: w.serviceInstanceId, target: "server" } }
        : null;
    case "addServer":
      return { kind: "servers" };
    case "openServer":
      return { kind: "server", nodeId: a.nodeId };
    case "openVariables":
      return w.serviceId ? { kind: "service", serviceId: w.serviceId, tab: "variables" } : { kind: "sharedVariables" };
  }
}

/** "1 warning", "4 warnings". */
export function warningsLabel(n: number): string {
  return `${n} warning${n === 1 ? "" : "s"}`;
}

/** The worst severity present (the button's tone). */
export function worstSeverity(items: EnvWarning[]): EnvWarning["severity"] | null {
  for (const s of ["error", "warning", "info"] as const) if (items.some((w) => w.severity === s)) return s;
  return null;
}
