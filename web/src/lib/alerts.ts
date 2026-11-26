import { del, get, patch, post, request } from "./api";

export type ChannelKind = "slack" | "discord" | "webhook" | "email";

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
  | "deployment_degraded"
  | "backup_failed"
  | "backup_completed"
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

export type SendPhase = "fired" | "resolved" | "test" | "report";

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

/** What one delivery to a channel came back with (a test, or one of a report's channels). */
export interface DeliveryResult {
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
  testChannel: (id: string) => post<DeliveryResult>(`/api/notification-channels/${enc(id)}/test`),
  rules: (workspace: string) => get<Rule[]>(`/api/workspaces/${enc(workspace)}/alert-rules`),
  projectRules: (project: string) => get<Rule[]>(`/api/projects/${enc(project)}/alert-rules`),
  createRule: (workspace: string, b: NewRule) => post<Rule>(`/api/workspaces/${enc(workspace)}/alert-rules`, b),
  updateRule: (id: string, b: RulePatch) => patch<Rule>(`/api/alert-rules/${enc(id)}`, b),
  deleteRule: (id: string) => del<void>(`/api/alert-rules/${enc(id)}`),
  history: (workspace: string) => get<Send[]>(`/api/workspaces/${enc(workspace)}/alert-history`),
  trafficReport: (workspace: string) => get<TrafficReport>(`/api/workspaces/${enc(workspace)}/traffic-report`),
  setTrafficReport: (workspace: string, b: TrafficReportBody) =>
    request<TrafficReport>("PUT", `/api/workspaces/${enc(workspace)}/traffic-report`, b),
  /** Sends the last 7 days now to the report's saved channels; one result per channel. */
  sendTrafficReport: (workspace: string) => post<DeliveryResult[]>(`/api/workspaces/${enc(workspace)}/traffic-report/send`),
};

export interface ChannelKindInfo {
  value: ChannelKind;
  label: string;
  /** The add-channel field: what it is called, what it holds and what it looks like. */
  inputLabel: string;
  input: "url" | "email";
  placeholder: string;
  /** Shown under the field when the kind needs something set up elsewhere. */
  hint?: string;
  /** The dialog's one-line description. */
  describe: string;
}

export const CHANNEL_KINDS: ChannelKindInfo[] = [
  {
    value: "slack",
    label: "Slack",
    inputLabel: "URL",
    input: "url",
    placeholder: "https://hooks.slack.com/services/...",
    describe: "Alerts are posted to this URL.",
  },
  {
    value: "discord",
    label: "Discord",
    inputLabel: "URL",
    input: "url",
    placeholder: "https://discord.com/api/webhooks/...",
    describe: "Alerts are posted to this URL.",
  },
  {
    value: "webhook",
    label: "Webhook",
    inputLabel: "URL",
    input: "url",
    placeholder: "https://example.com/hooks/modula",
    describe: "Alerts are posted to this URL.",
  },
  {
    value: "email",
    label: "Email",
    inputLabel: "Email address",
    input: "email",
    placeholder: "you@example.com",
    hint: "Sent through this workspace's Cloudflare account. It needs a sender address under Integrations → Cloudflare.",
    describe: "Alerts are emailed to this address.",
  },
];

export function channelKindInfo(kind: string): ChannelKindInfo | undefined {
  return CHANNEL_KINDS.find((k) => k.value === kind);
}

export function channelKindLabel(kind: string): string {
  return channelKindInfo(kind)?.label ?? kind;
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
  { value: "deployment_degraded", label: "Deployment degraded", hint: "A replica crashed while the others keep serving.", threshold: false, window: null },
  { value: "backup_failed", label: "Backup failed", hint: "A database backup failed.", threshold: false, window: null },
  {
    value: "backup_completed",
    label: "Backup completed",
    hint: "A database backup finished: size, duration and destination.",
    threshold: false,
    window: null,
  },
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

/** The cooldown a new rule starts with: none for finished backups (each one is worth a note). */
export function defaultCooldown(kind: RuleKind): number {
  return kind === "backup_completed" ? 0 : DEFAULT_COOLDOWN_MINUTES;
}

/** The form after picking `kind`: an untouched cooldown follows the kind's default. */
export function withKind(f: RuleForm, kind: RuleKind): Partial<RuleForm> {
  const untouched = f.cooldownMinutes === String(defaultCooldown(f.kind));
  return untouched ? { kind, cooldownMinutes: String(defaultCooldown(kind)) } : { kind };
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

/**
 * True for an email address the server will take: one `@`, no spaces or `<>,;"`, a dot in the domain,
 * at most 254 characters. The server checks it again.
 */
export function isEmailAddress(s: string): boolean {
  const a = s.trim();
  if (a.length === 0 || a.length > 254 || /[\s<>,;"]/.test(a)) return false;
  const parts = a.split("@");
  const [local, domain] = parts;
  return parts.length === 2 && !!local && !!domain && domain.includes(".");
}

/** Whether `value` is a usable target for the kind: an address for email, an http(s) URL for the rest. */
export function isChannelTarget(kind: ChannelKind, value: string): boolean {
  return kind === "email" ? isEmailAddress(value) : isChannelUrl(value);
}

type StatusTone = "neutral" | "success" | "danger";

export interface StatusBadge {
  label: string;
  tone: StatusTone;
  /** A failure's explanation when it is too long for the badge; show it as text beside or under it. */
  detail?: string;
}

/** Errors longer than this don't fit a badge (email failures are whole sentences). */
const BADGE_ERROR_MAX = 32;

function statusOf(status: number | null, error: string | null): StatusBadge {
  if (status !== null && status >= 200 && status < 300) return { label: `${status} OK`, tone: "success" };
  if (status !== null) return error ? { label: String(status), tone: "danger", detail: error } : { label: String(status), tone: "danger" };
  if (!error) return { label: "Failed", tone: "danger" };
  return error.length <= BADGE_ERROR_MAX ? { label: error, tone: "danger" } : { label: "Failed", tone: "danger", detail: error };
}

/** A channel's last delivery as a badge label and tone. */
export function channelStatus(c: Pick<Channel, "lastStatus" | "lastError" | "lastAt">): StatusBadge {
  if (!c.lastAt) return { label: "Never sent", tone: "neutral" };
  return statusOf(c.lastStatus, c.lastError);
}

/** A history row's outcome as a badge label and tone. */
export function sendStatus(s: Pick<Send, "status" | "error">): StatusBadge {
  return statusOf(s.status, s.error);
}

export function phaseLabel(p: SendPhase): string {
  switch (p) {
    case "fired":
      return "Fired";
    case "resolved":
      return "Resolved";
    case "report":
      return "Report";
    default:
      return "Test";
  }
}

/** The history's "Rule" cell: the rule's name, or what a send without one was. */
export function sendSubject(s: Pick<Send, "ruleName" | "phase">): string {
  if (s.ruleName) return s.ruleName;
  return s.phase === "report" ? "Weekly traffic report" : "Test message";
}

function isOk(r: DeliveryResult): boolean {
  return r.status !== null && r.status >= 200 && r.status < 300;
}

/** `text` as a sentence: a full stop is added unless it already ends with punctuation. */
function sentence(text: string): string {
  const t = text.trim();
  return /[.!?]$/.test(t) ? t : `${t}.`;
}

/** Why a delivery failed, as a sentence. */
function failure(r: DeliveryResult): string {
  if (r.error) return sentence(r.error);
  return r.status !== null ? `the receiver answered ${r.status}.` : "no response.";
}

/** The message shown after "Send test". Email has no receiver answer to quote, only the reason it failed. */
export function testResultMessage(r: DeliveryResult, kind?: string): { ok: boolean; text: string } {
  if (kind === "email") {
    if (isOk(r)) return { ok: true, text: "Test email sent." };
    return { ok: false, text: `Test failed: ${failure(r)}` };
  }
  if (r.status !== null && r.status >= 200 && r.status < 300) return { ok: true, text: `Test sent, the receiver answered ${r.status}.` };
  if (r.status !== null) return { ok: false, text: `Test failed, the receiver answered ${r.status}.` };
  return { ok: false, text: `Test failed: ${r.error ?? "no response"}.` };
}

// ---------------------------------------------------------------- weekly traffic report

export interface TrafficReport {
  enabled: boolean;
  /** IANA name; "UTC" until the report is set up. */
  timezone: string;
  channelIds: string[];
  /** The next run (RFC 3339). Also set while the report is off; null until it was saved once. */
  nextAt: string | null;
  lastSentAt: string | null;
}

export interface TrafficReportBody {
  enabled: boolean;
  timezone?: string;
  channelIds?: string[];
}

/** The report form's state, before it is saved. */
export interface ReportDraft {
  enabled: boolean;
  timezone: string;
  channelIds: string[];
}

/** A report nobody has saved yet: the server answers with defaults and no schedule. */
export function reportNeverSet(r: Pick<TrafficReport, "enabled" | "nextAt" | "lastSentAt">): boolean {
  return !r.enabled && r.nextAt === null && r.lastSentAt === null;
}

/** The form for a saved report; a report never set up starts in the browser's timezone, not the server's UTC. */
export function reportDraft(r: TrafficReport, browserTz: string): ReportDraft {
  return { enabled: r.enabled, timezone: reportNeverSet(r) ? browserTz : r.timezone, channelIds: [...r.channelIds] };
}

function sameIds(a: string[], b: string[]): boolean {
  const sorted = [...b].sort();
  return a.length === b.length && [...a].sort().every((id, i) => id === sorted[i]);
}

/** True when the form differs from what is saved. */
export function reportDirty(d: ReportDraft, r: TrafficReport, browserTz: string): boolean {
  const saved = reportDraft(r, browserTz);
  return d.enabled !== saved.enabled || d.timezone !== saved.timezone || !sameIds(d.channelIds, saved.channelIds);
}

/** The first problem with the form, or null when it can be saved. */
export function validateReportDraft(d: ReportDraft): string | null {
  if (d.enabled && d.channelIds.length === 0) return "Pick at least one channel.";
  return null;
}

/** The request body for the form; channels that no longer exist are dropped. */
export function reportBody(d: ReportDraft, knownChannelIds: string[]): TrafficReportBody {
  return { enabled: d.enabled, timezone: d.timezone, channelIds: d.channelIds.filter((id) => knownChannelIds.includes(id)) };
}

/** Timezones offered by the select when the browser can't list them. */
export const FALLBACK_TIMEZONES = [
  "UTC",
  "America/Los_Angeles",
  "America/Denver",
  "America/Chicago",
  "America/New_York",
  "America/Sao_Paulo",
  "Europe/London",
  "Europe/Paris",
  "Europe/Berlin",
  "Europe/Istanbul",
  "Africa/Cairo",
  "Africa/Johannesburg",
  "Asia/Dubai",
  "Asia/Kolkata",
  "Asia/Bangkok",
  "Asia/Singapore",
  "Asia/Shanghai",
  "Asia/Tokyo",
  "Australia/Sydney",
  "Pacific/Auckland",
];

/** The browser's own timezone, or UTC when it can't tell. */
export function browserTimezone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
}

/** Every IANA timezone the browser knows, or a short list on browsers without `Intl.supportedValuesOf`. */
export function supportedTimezones(): string[] {
  try {
    const list = typeof Intl.supportedValuesOf === "function" ? Intl.supportedValuesOf("timeZone") : [];
    if (list.length > 0) return list;
  } catch {
    /* fall through to the short list */
  }
  return FALLBACK_TIMEZONES;
}

/**
 * The select's options: UTC first, then the supported zones, always including `current` (browsers leave
 * out UTC and legacy names that a saved report or the browser itself may use).
 */
export function timezoneOptions(supported: string[], current: string): string[] {
  const rest = new Set(supported);
  if (current) rest.add(current);
  rest.delete("UTC");
  return ["UTC", ...[...rest].sort()];
}

/** An instant in `timeZone` (default: the browser's), e.g. "Mon, 30 Nov, 09:00 GMT+5:30". */
export function formatLocal(iso: string, locale?: string, timeZone?: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(locale, {
    weekday: "short",
    day: "numeric",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
    timeZoneName: "short",
    timeZone,
  }).format(d);
}

/**
 * When reports go out, e.g. "Every Monday at 09:00 (Asia/Kolkata) · next: Mon, 30 Nov, 09:00 GMT+5:30".
 * Pass `nextAt` only when it applies to this timezone and the report is on.
 */
export function reportSchedule(timezone: string, nextAt: string | null, locale?: string, timeZone?: string): string {
  const base = `Every Monday at 09:00 (${timezone})`;
  return nextAt ? `${base} · next: ${formatLocal(nextAt, locale, timeZone)}` : base;
}

export interface ReportDelivery {
  name: string;
  ok: boolean;
  text: string;
}

/**
 * What "Send now" did, one line per channel. The server answers with bare results, in the order of the
 * report's saved channels (oldest first, like the channel list); if the counts don't line up the lines are
 * numbered instead of named.
 */
export function reportDeliveries(
  channels: Pick<Channel, "id" | "name">[],
  savedIds: string[],
  results: DeliveryResult[],
): ReportDelivery[] {
  const saved = channels.filter((c) => savedIds.includes(c.id));
  const named = saved.length === results.length;
  return results.map((r, i) => {
    const name = named ? (saved[i]?.name ?? `Channel ${i + 1}`) : `Channel ${i + 1}`;
    return isOk(r) ? { name, ok: true, text: `${name}: sent.` } : { name, ok: false, text: `${name}: ${failure(r)}` };
  });
}
