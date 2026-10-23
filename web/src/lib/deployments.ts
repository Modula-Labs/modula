import type { Deployment } from "./api";

/** "via GitHub", "via Docker image", "via Rollback"… for a deployment's subtitle. */
export function viaLabel(d: Pick<Deployment, "trigger" | "sourceKind" | "commitSha" | "image">): string {
  if (d.trigger === "rollback") return "via Rollback";
  if (d.trigger === "redeploy") return "via Redeploy";
  if (d.trigger === "restart") return "via Restart";
  const kind = d.sourceKind ?? (d.commitSha ? "github" : "image");
  return kind === "github" ? "via GitHub" : kind === "upload" ? "via Upload" : "via Docker image";
}

/** First line of the commit message, or the image reference. */
export function deploymentTitle(d: Pick<Deployment, "commitMessage" | "commitSha" | "image">): string {
  const line = d.commitMessage?.split("\n")[0]?.trim();
  if (line) return line;
  return d.commitSha ? d.commitSha.slice(0, 7) : d.image;
}
