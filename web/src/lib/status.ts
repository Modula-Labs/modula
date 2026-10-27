import type { Tone } from "@/components/ui/misc";
import type { DeploymentStatus } from "./api";

/** How a deployment status reads in badges. */
export const deploymentBadge: Record<DeploymentStatus, { label: string; tone: Tone }> = {
  waiting_ci: { label: "Waiting for CI", tone: "neutral" },
  queued: { label: "Queued", tone: "neutral" },
  building: { label: "Building", tone: "strong" },
  deploying: { label: "Deploying", tone: "strong" },
  active: { label: "Active", tone: "success" },
  sleeping: { label: "Sleeping", tone: "neutral" },
  failed: { label: "Failed", tone: "danger" },
  crashed: { label: "Crashed", tone: "danger" },
  removed: { label: "Removed", tone: "neutral" },
  cancelled: { label: "Cancelled", tone: "neutral" },
};

/** A deployment's badge: its status, or Degraded while it is active with a replica down. */
export function badgeOf(d: { status: DeploymentStatus; degraded?: boolean | null }): { label: string; tone: Tone } {
  return d.status === "active" && d.degraded ? { label: "Degraded", tone: "warn" } : deploymentBadge[d.status];
}

/** The one-line service state shown on canvas cards. */
export function serviceState(
  status: DeploymentStatus | null,
  degraded?: boolean | null,
): {
  label: string;
  tone: "success" | "warn" | "danger" | "neutral";
} {
  switch (status) {
    case "active":
      return degraded ? { label: "Degraded", tone: "warn" } : { label: "Online", tone: "success" };
    case "sleeping":
      return { label: "Sleeping", tone: "neutral" };
    case "building":
      return { label: "Building", tone: "warn" };
    case "deploying":
    case "queued":
      return { label: "Deploying", tone: "warn" };
    case "waiting_ci":
      return { label: "Waiting for CI", tone: "warn" };
    case "crashed":
      return { label: "Crashed", tone: "danger" };
    case "failed":
      return { label: "Deploy failed", tone: "danger" };
    default:
      return { label: "Service is offline", tone: "neutral" };
  }
}

export const isLive = (s: DeploymentStatus) =>
  s === "waiting_ci" || s === "queued" || s === "building" || s === "deploying";

/** The deployment is the one serving traffic: running, crashed, or scaled to zero. */
export const isServingStatus = (s: DeploymentStatus) => s === "active" || s === "crashed" || s === "sleeping";
