import type { Deployment } from "./api";

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
