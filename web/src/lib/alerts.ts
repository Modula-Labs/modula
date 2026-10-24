import { del, get, patch, post } from "./api";

export type ChannelKind = "slack" | "discord" | "webhook";

export interface Channel {
  id: string;
  kind: ChannelKind;
  name: string;
  host: string;
  createdAt: string;
  lastStatus: number | null;
  lastError: string | null;
  lastAt: string | null;
}

export type RuleKind =
  | "deployment_failed"
  | "build_failed"
  | "deployment_crashed"
  | "backup_failed"
  | "cpu"
  | "memory"
  | "disk"
  | "node_offline";

export interface Rule {
  id: string;
  name: string;
  kind: RuleKind;
  projectId: string | null;
  projectName: string | null;
  serviceId: string | null;
  serviceName: string | null;
  threshold: number | null;
  windowMinutes: number | null;
  cooldownMinutes: number;
  enabled: boolean;
  channelIds: string[];
  firing: number;
  createdAt: string;
}

export type SendPhase = "fired" | "resolved" | "test";

export interface Send {
  id: number;
  at: string;
  ruleId: string | null;
  ruleName: string;
  channelId: string | null;
  channelName: string;
  channelKind: string;
  phase: SendPhase;
  subject: string;
  title: string;
  status: number | null;
  error: string | null;
}

export interface NewChannel {
  kind: ChannelKind;
  name: string;
  url: string;
}

export interface NewRule {
  name: string;
  kind: RuleKind;
  projectId?: string;
  serviceId?: string;
  threshold?: number;
  windowMinutes?: number;
  cooldownMinutes?: number;
  channelIds: string[];
}

export type RulePatch = Partial<Pick<Rule, "name" | "enabled" | "threshold" | "windowMinutes" | "cooldownMinutes" | "channelIds">>;

const enc = encodeURIComponent;

export const alertsApi = {
  channels: (workspace: string) => get<Channel[]>(`/api/workspaces/${enc(workspace)}/notification-channels`),
  /** The signing secret (webhook channels only) is returned just here. */
  createChannel: (workspace: string, b: NewChannel) =>
    post<Channel & { secret?: string }>(`/api/workspaces/${enc(workspace)}/notification-channels`, b),
  deleteChannel: (id: string) => del<void>(`/api/notification-channels/${enc(id)}`),
  testChannel: (id: string) => post<{ status: number | null; error: string | null }>(`/api/notification-channels/${enc(id)}/test`),
  rules: (workspace: string) => get<Rule[]>(`/api/workspaces/${enc(workspace)}/alert-rules`),
  projectRules: (project: string) => get<Rule[]>(`/api/projects/${enc(project)}/alert-rules`),
  createRule: (workspace: string, b: NewRule) => post<Rule>(`/api/workspaces/${enc(workspace)}/alert-rules`, b),
  updateRule: (id: string, b: RulePatch) => patch<Rule>(`/api/alert-rules/${enc(id)}`, b),
  deleteRule: (id: string) => del<void>(`/api/alert-rules/${enc(id)}`),
  history: (workspace: string) => get<Send[]>(`/api/workspaces/${enc(workspace)}/alert-history`),
};

export const CHANNEL_KINDS: { value: ChannelKind; label: string; placeholder: string }[] = [
  { value: "slack", label: "Slack", placeholder: "https://hooks.slack.com/services/..." },
  { value: "discord", label: "Discord", placeholder: "https://discord.com/api/webhooks/..." },
  { value: "webhook", label: "Webhook", placeholder: "https://example.com/hooks/modula" },
];

export function channelKindLabel(kind: string): string {
  return CHANNEL_KINDS.find((k) => k.value === kind)?.label ?? kind;
}

export function urlPlaceholder(kind: ChannelKind): string {
  return CHANNEL_KINDS.find((k) => k.value === kind)?.placeholder ?? "https://";
}

/** What a rule kind needs beyond its scope: a threshold and/or a window, with the window's allowed range. */
export interface RuleKindInfo {
  value: RuleKind;
  label: string;
  hint: string;
  threshold: boolean;
  window: { min: number; max: number } | null;
}

export const RULE_KINDS: RuleKindInfo[] = [
  { value: "deployment_failed", label: "Deployment failed", hint: "A deployment failed after the build.", threshold: false, window: null },
  { value: "build_failed", label: "Build failed", hint: "A build failed.", threshold: false, window: null },
  { value: "deployment_crashed", label: "Deployment crashed", hint: "A running deployment exited unexpectedly.", threshold: false, window: null },
  { value: "backup_failed", label: "Backup failed", hint: "A database backup failed.", threshold: false, window: null },
  {
    value: "cpu",
    label: "High CPU",
    hint: "CPU use stays above the threshold, as a percentage of the service's CPU limit or, without one, of the server's CPUs.",
    threshold: true,
    window: { min: 1, max: 60 },
  },
  {
    value: "memory",
    label: "High memory",
    hint: "Memory use stays above the threshold, as a percentage of the service's memory limit. Services without a memory limit are not evaluated.",
    threshold: true,
    window: { min: 1, max: 60 },
  },
  {
    value: "disk",
    label: "Disk usage",
    hint: "The server's disk holding Modula's data and volumes is fuller than the threshold.",
    threshold: true,
    window: null,
  },
  { value: "node_offline", label: "Server offline", hint: "A server has been offline for the given time.", threshold: false, window: { min: 1, max: 1440 } },
];

export const DEFAULT_COOLDOWN_MINUTES = 30;
export const MAX_COOLDOWN_MINUTES = 1440;

export function ruleKindInfo(kind: RuleKind): RuleKindInfo {
  return RULE_KINDS.find((k) => k.value === kind) ?? RULE_KINDS[0]!;
}

/** Only kinds about a service can be scoped to one; disk and server rules are per server. */
export function kindSupportsServiceScope(kind: RuleKind): boolean {
  return kind !== "disk" && kind !== "node_offline";
}

function minutes(n: number): string {
  if (n % 1440 === 0) return n === 1440 ? "1 day" : `${n / 1440} days`;
  if (n >= 120 && n % 60 === 0) return `${n / 60} h`;
  return `${n} min`;
}

/** A rule in one line, e.g. "CPU above 80% for 5 min". */
export function describeRule(r: Pick<Rule, "kind" | "threshold" | "windowMinutes">): string {
  const win = r.windowMinutes !== null ? ` for ${minutes(r.windowMinutes)}` : "";
  switch (r.kind) {
    case "cpu":
      return `CPU above ${r.threshold ?? "?"}%${win}`;
    case "memory":
      return `Memory above ${r.threshold ?? "?"}% of limit${win}`;
    case "disk":
      return `Disk above ${r.threshold ?? "?"}% used`;
    case "node_offline":
      return `Server offline${win}`;
    default:
      return ruleKindInfo(r.kind).label;
  }
}

/** Where a rule applies: the whole workspace, a project, or one service of a project. */
export function ruleScope(r: Pick<Rule, "projectId" | "projectName" | "serviceId" | "serviceName">): string {
  if (r.serviceId) return `${r.projectName ?? "Project"} / ${r.serviceName ?? "service"}`;
  if (r.projectId) return r.projectName ?? "Project";
  return "Whole workspace";
}

export interface RuleForm {
  name: string;
  kind: RuleKind;
  projectId: string;
  serviceId: string;
  threshold: string;
  windowMinutes: string;
  cooldownMinutes: string;
  channelIds: string[];
}

export function emptyRuleForm(): RuleForm {
  return {
    name: "",
    kind: "deployment_failed",
    projectId: "",
    serviceId: "",
    threshold: "80",
    windowMinutes: "5",
    cooldownMinutes: String(DEFAULT_COOLDOWN_MINUTES),
    channelIds: [],
  };
}

function intIn(s: string, min: number, max: number): number | null {
  if (!/^\d+$/.test(s.trim())) return null;
  const n = Number(s);
  return n >= min && n <= max ? n : null;
}

/** The first problem with the add-rule form, or null when it can be submitted. */
export function validateRuleForm(f: RuleForm): string | null {
  if (!f.name.trim()) return "Give the rule a name.";
  const info = ruleKindInfo(f.kind);
  if (info.threshold && intIn(f.threshold, 1, 100) === null) return "Threshold must be a whole number from 1 to 100.";
  if (info.window && intIn(f.windowMinutes, info.window.min, info.window.max) === null)
    return `Window must be a whole number of minutes from ${info.window.min} to ${info.window.max}.`;
  if (intIn(f.cooldownMinutes, 0, MAX_COOLDOWN_MINUTES) === null) return `Cooldown must be a whole number of minutes from 0 to ${MAX_COOLDOWN_MINUTES}.`;
  if (f.serviceId && !f.projectId) return "Pick a project for the service.";
  if (f.channelIds.length === 0) return "Pick at least one channel.";
  return null;
}

/** The request body for a form that passed `validateRuleForm`; fields the kind doesn't use are left out. */
export function ruleFormBody(f: RuleForm): NewRule {
  const info = ruleKindInfo(f.kind);
  const body: NewRule = { name: f.name.trim(), kind: f.kind, cooldownMinutes: Number(f.cooldownMinutes), channelIds: f.channelIds };
  if (f.projectId) body.projectId = f.projectId;
  if (f.serviceId && kindSupportsServiceScope(f.kind)) body.serviceId = f.serviceId;
  if (info.threshold) body.threshold = Number(f.threshold);
  if (info.window) body.windowMinutes = Number(f.windowMinutes);
  return body;
}

/** True for an http(s) URL; the server checks it again. */
export function isChannelUrl(s: string): boolean {
  try {
    const u = new URL(s.trim());
    return u.protocol === "http:" || u.protocol === "https:";
  } catch {
    return false;
  }
}

type StatusTone = "neutral" | "success" | "danger";

function statusOf(status: number | null, error: string | null): { label: string; tone: StatusTone } {
  if (status !== null && status >= 200 && status < 300) return { label: `${status} OK`, tone: "success" };
  if (status !== null) return { label: String(status), tone: "danger" };
  return { label: error || "Failed", tone: "danger" };
}

/** A channel's last delivery as a badge label and tone. */
export function channelStatus(c: Pick<Channel, "lastStatus" | "lastError" | "lastAt">): { label: string; tone: StatusTone } {
  if (!c.lastAt) return { label: "Never sent", tone: "neutral" };
  return statusOf(c.lastStatus, c.lastError);
}

/** A history row's outcome as a badge label and tone. */
export function sendStatus(s: Pick<Send, "status" | "error">): { label: string; tone: StatusTone } {
  return statusOf(s.status, s.error);
}

export function phaseLabel(p: SendPhase): string {
  return p === "fired" ? "Fired" : p === "resolved" ? "Resolved" : "Test";
}

/** The message shown after "Send test". */
export function testResultMessage(r: { status: number | null; error: string | null }): { ok: boolean; text: string } {
  if (r.status !== null && r.status >= 200 && r.status < 300) return { ok: true, text: `Test sent, the receiver answered ${r.status}.` };
  if (r.status !== null) return { ok: false, text: `Test failed, the receiver answered ${r.status}.` };
  return { ok: false, text: `Test failed: ${r.error ?? "no response"}.` };
}
