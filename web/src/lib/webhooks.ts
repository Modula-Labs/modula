import type { Webhook, WebhookEvent } from "./api";

export const WEBHOOK_EVENTS: { value: WebhookEvent; label: string; hint: string }[] = [
  { value: "deployment.succeeded", label: "Deployment succeeded", hint: "A deployment became active." },
  { value: "deployment.failed", label: "Deployment failed", hint: "A build or start failed." },
  { value: "deployment.crashed", label: "Deployment crashed", hint: "A running deployment exited unexpectedly." },
];

/** True for an http(s) URL; the server checks it again. */
export function isWebhookUrl(s: string): boolean {
  try {
    const u = new URL(s.trim());
    return u.protocol === "http:" || u.protocol === "https:";
  } catch {
    return false;
  }
}

/** The last delivery as a short label and a tone for its badge. */
export function deliveryStatus(w: Pick<Webhook, "lastStatus" | "lastError" | "lastAt">): { label: string; tone: "neutral" | "success" | "danger" } {
  if (!w.lastAt) return { label: "Never sent", tone: "neutral" };
  if (w.lastStatus !== null && w.lastStatus >= 200 && w.lastStatus < 300) return { label: `${w.lastStatus} OK`, tone: "success" };
  if (w.lastStatus !== null) return { label: String(w.lastStatus), tone: "danger" };
  return { label: w.lastError || "Failed", tone: "danger" };
}
