import type { Tone } from "@/components/ui/misc";
import type { DeploymentStatus } from "./api";

/** How a deployment status reads in badges. */
export const deploymentBadge: Record<DeploymentStatus, { label: string; tone: Tone }> = {
  queued: { label: "Queued", tone: "neutral" },
  building: { label: "Building", tone: "strong" },
  deploying: { label: "Deploying", tone: "strong" },
  active: { label: "Active", tone: "success" },
  failed: { label: "Failed", tone: "danger" },
  crashed: { label: "Crashed", tone: "danger" },
  removed: { label: "Removed", tone: "neutral" },
  cancelled: { label: "Cancelled", tone: "neutral" },
};

/** The one-line service state shown on canvas cards. */
export function serviceState(status: DeploymentStatus | null): {
  label: string;
  tone: "success" | "warn" | "danger" | "neutral";
} {
  switch (status) {
    case "active":
      return { label: "Online", tone: "success" };
    case "building":
      return { label: "Building", tone: "warn" };
    case "deploying":
    case "queued":
      return { label: "Deploying", tone: "warn" };
    case "crashed":
      return { label: "Crashed", tone: "danger" };
    case "failed":
      return { label: "Deploy failed", tone: "danger" };
    default:
      return { label: "Service is offline", tone: "neutral" };
  }
}

export const isLive = (s: DeploymentStatus) => s === "queued" || s === "building" || s === "deploying";
