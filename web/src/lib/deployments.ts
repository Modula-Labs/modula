import type { Deployment } from "./api";
import { formatMs, relativeTime } from "./format";

type ViaFields = "trigger" | "sourceKind" | "commitSha" | "image" | "sourceBranch" | "sourceDirty";

/** "via GitHub", "via Docker image", "Uploaded via CLI from main@abc1234"… for a deployment's subtitle. */
export function viaLabel(d: Pick<Deployment, "trigger" | "sourceKind" | "commitSha" | "image"> & Partial<Pick<Deployment, ViaFields>>): string {
  if (d.trigger === "rollback") return "via Rollback";
  if (d.trigger === "redeploy") return "via Redeploy";
  if (d.trigger === "restart") return "via Restart";
  const kind = d.sourceKind ?? (d.commitSha ? "github" : "image");
  if (kind === "upload") return `Uploaded via CLI${uploadOrigin(d)}`;
  return kind === "github" ? "via GitHub" : "via Docker image";
}

/** " from main@abc1234 with uncommitted changes": the checkout an upload was packed from. */
function uploadOrigin(d: Partial<Pick<Deployment, ViaFields>>): string {
  const at = [d.sourceBranch, d.commitSha?.slice(0, 7)].filter(Boolean).join("@");
  return `${at ? ` from ${at}` : ""}${d.sourceDirty ? " with uncommitted changes" : ""}`;
}

/** First line of the commit message (or upload message), else the commit or image. */
export function deploymentTitle(d: Pick<Deployment, "commitMessage" | "commitSha" | "image"> & Partial<Pick<Deployment, "sourceKind">>): string {
  const line = d.commitMessage?.split("\n")[0]?.trim();
  if (line) return line;
  if (d.commitSha) return d.commitSha.slice(0, 7);
  return d.sourceKind === "upload" ? "Local files" : d.image;
}

/** "Sleeping · wakes on request", plus "Last cold start 412 ms · 3m ago" once it has woken before. */
export function sleepingLine(d: Pick<Deployment, "wokeAt" | "coldStartMs">, now = Date.now()): string {
  const cold =
    d.coldStartMs != null
      ? ` · Last cold start ${formatMs(d.coldStartMs)}${d.wokeAt ? ` · ${relativeTime(d.wokeAt, now)}` : ""}`
      : "";
  return `Sleeping · wakes on request${cold}`;
}
