import type { ActivityChange, ActivityGroup } from "@/lib/api";

/** `startCommand` -> `start command`. */
export function words(key: string): string {
  return key
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/[_-]+/g, " ")
    .toLowerCase();
}

const deploymentVerb: Record<string, string> = {
  waiting_ci: "is waiting for CI",
  queued: "queued",
  building: "started building",
  deploying: "is deploying",
  active: "succeeded",
  sleeping: "went to sleep",
  failed: "failed",
  crashed: "crashed",
  removed: "was removed",
  cancelled: "was cancelled",
};

/** One human line per change, e.g. "Added variable PORT" or "Deployment failed (fix build)". */
export function describeChange(c: ActivityChange): string {
  switch (c.kind) {
    case "variable": {
      const verb = c.op === "added" || c.op === "created" ? "Added" : c.op === "removed" || c.op === "deleted" ? "Removed" : "Updated";
      return c.key ? `${verb} variable ${c.key}` : `${verb} variables`;
    }
    case "config":
      return c.key ? `Changed ${words(c.key)}` : "Changed settings";
    case "deployment": {
      const verb = (c.status && deploymentVerb[c.status]) ?? "updated";
      const msg = c.message?.split("\n")[0]?.trim();
      return `Deployment ${verb}${msg ? ` (${msg})` : ""}`;
    }
    case "service":
      return c.op === "deleted" || c.op === "removed" ? "Deleted service" : "Created service";
  }
}

export function groupTitle(g: Pick<ActivityGroup, "serviceName" | "changes">): string {
  const n = g.changes.length;
  return `${n} ${n === 1 ? "change" : "changes"} in ${g.serviceName ?? "project"}`;
}

export function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  return ((parts[0]?.[0] ?? "?") + (parts.length > 1 ? parts[parts.length - 1]![0]! : "")).toUpperCase();
}
