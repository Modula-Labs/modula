import type { EdgeAction, EdgeCondition, EdgeRule, ServiceConfig } from "./api";
import { HTTP_METHODS } from "./http";

export const MAX_EDGE_RULES = 10;
export const MAX_RULE_NAME = 60;
export const MAX_OR_GROUPS = 5;
export const MAX_AND_CONDITIONS = 5;
export const MAX_BODY_BYTES = 4096;
export const MAX_CACHE_TTL_SECS = 604_800;

export type ConditionField = EdgeCondition["field"];
export type ActionType = EdgeAction["type"];

export const CONDITION_FIELDS: { value: ConditionField; label: string }[] = [
  { value: "path", label: "Path" },
  { value: "method", label: "Method" },
  { value: "header", label: "Header" },
  { value: "ip", label: "Client IP" },
  { value: "country", label: "Country" },
];

export const ACTION_TYPES: { value: ActionType; label: string }[] = [
  { value: "block", label: "Block" },
  { value: "allow", label: "Allow" },
  { value: "redirect", label: "Redirect" },
  { value: "rateLimit", label: "Rate limit" },
  { value: "headers", label: "Set headers" },
  { value: "cache", label: "Cache" },
];

export const REDIRECT_STATUSES = [301, 302, 307, 308] as const;

/** A fresh enabled rule: every request, blocked with 403. */
export const newRule = (): EdgeRule => ({ name: "", enabled: true, when: [], then: { type: "block", status: 403 } });

export function defaultCondition(field: ConditionField, negate?: boolean): EdgeCondition {
  const n = negate ? { negate: true } : {};
  switch (field) {
    case "path":
      return { field, op: "matches", value: "", ...n };
    case "method":
      return { field, values: [], ...n };
    case "header":
      return { field, name: "", op: "equals", value: "", ...n };
    case "ip":
      return { field, cidrs: [], ...n };
    case "country":
      return { field, values: [], ...n };
  }
}

export function defaultAction(type: ActionType): EdgeAction {
  switch (type) {
    case "rateLimit":
      return { type, requests: 100, windowSecs: 60, key: "ip" };
    case "block":
      return { type, status: 403 };
    case "allow":
      return { type };
    case "redirect":
      return { type, status: 302, target: "" };
    case "headers":
      return { type, set: {} };
    case "cache":
      return { type, ttlSecs: 300 };
  }
}

/** Block, redirect and allow end the evaluation; the others only add behaviour. */
export const isTerminal = (a: EdgeAction): boolean => a.type === "block" || a.type === "redirect" || a.type === "allow";

/** "a, b ,,c" -> ["a","b","c"]. */
export const parseList = (s: string): string[] =>
  s
    .split(/[,\n]/)
    .map((x) => x.trim())
    .filter(Boolean);

/** 1 -> "1s", 90 -> "90s", 60 -> "1m", 3600 -> "1h". */
export function formatSecs(n: number): string {
  if (n >= 3600 && n % 3600 === 0) return `${n / 3600}h`;
  if (n >= 60 && n % 60 === 0) return `${n / 60}m`;
  return `${n}s`;
}

/** The action as a short label with its main parameter: "Block 403", "Rate limit 100/1m", "Cache 5m". */
export function describeAction(a: EdgeAction): string {
  switch (a.type) {
    case "rateLimit":
      return `Rate limit ${a.requests}/${formatSecs(a.windowSecs)}`;
    case "block":
      return `Block ${a.status}`;
    case "allow":
      return "Allow";
    case "redirect":
      return `Redirect ${a.status}`;
    case "headers":
      return "Set headers";
    case "cache":
      return `Cache ${formatSecs(a.ttlSecs)}`;
  }
}

/** The label of an action type, as the "unsupported" notice names it ("rateLimit" -> "Rate limit"). */
export const actionLabel = (type: string): string => ACTION_TYPES.find((a) => a.value === type)?.label ?? type;

/** "Rate limit rules aren't applied on one of this service's servers: its edge has no rate-limit module." */
export function unsupportedMessage(types: string[]): string {
  const names = types.map((t) => actionLabel(t).toLowerCase());
  const list = names.length > 1 ? `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}` : (names[0] ?? "");
  const cap = list.charAt(0).toUpperCase() + list.slice(1);
  const modules = types.map((t) => (t === "rateLimit" ? "rate-limit" : t)).join(" or ");
  return `${cap} rules aren't applied on one of this service's servers: its edge has no ${modules} module.`;
}

// ---- validation ----------------------------------------------------------------------------

const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

const isIpv4 = (s: string) => {
  const p = s.split(".");
  return p.length === 4 && p.every((x) => /^\d{1,3}$/.test(x) && Number(x) <= 255);
};

function isIpv6(s: string): boolean {
  if (!/^[0-9a-fA-F:.]+$/.test(s) || !s.includes(":")) return false;
  const halves = s.split("::");
  if (halves.length > 2) return false;
  const groups = (h: string) => (h === "" ? [] : h.split(":"));
  const all = halves.flatMap(groups);
  let count = all.length;
  const last = all[all.length - 1];
  if (last?.includes(".")) {
    if (!isIpv4(last)) return false;
    count += 1; // an embedded IPv4 takes two groups
    all.pop();
  }
  if (!all.every((g) => /^[0-9a-fA-F]{1,4}$/.test(g))) return false;
  return halves.length === 2 ? count < 8 : count === 8;
}

/** A bare IPv4/IPv6 address or one with a /prefix. */
export function isIpOrCidr(s: string): boolean {
  const [addr, prefix, ...rest] = s.split("/");
  if (rest.length || !addr) return false;
  const v4 = isIpv4(addr);
  if (!v4 && !isIpv6(addr)) return false;
  if (prefix === undefined) return true;
  return /^\d{1,3}$/.test(prefix) && Number(prefix) <= (v4 ? 32 : 128);
}

const intIn = (n: number, min: number, max: number) => Number.isInteger(n) && n >= min && n <= max;
const byteLen = (s: string) => new TextEncoder().encode(s).length;

function conditionError(c: EdgeCondition): string | null {
  switch (c.field) {
    case "path":
      if (!c.value.startsWith("/")) return "A path condition's value must start with /";
      return null;
    case "method":
      if (c.values.length === 0) return "Pick at least one method";
      return c.values.every((m) => (HTTP_METHODS as readonly string[]).includes(m)) ? null : "Unknown HTTP method";
    case "header":
      if (!TOKEN.test(c.name)) return "A header condition needs a valid header name";
      if (c.op !== "exists" && !c.value) return `A header condition needs a value to ${c.op === "equals" ? "equal" : "contain"}`;
      return null;
    case "ip": {
      if (c.cidrs.length === 0) return "Add at least one IP address or range";
      const bad = c.cidrs.find((x) => !isIpOrCidr(x));
      return bad ? `"${bad}" isn't an IP address or CIDR range` : null;
    }
    case "country": {
      if (c.values.length === 0) return "Add at least one country code";
      const bad = c.values.find((x) => !/^[A-Za-z]{2}$/.test(x));
      return bad ? `"${bad}" isn't a two-letter country code` : null;
    }
  }
}

function actionError(a: EdgeAction): string | null {
  switch (a.type) {
    case "rateLimit":
      if (!intIn(a.requests, 1, 1_000_000)) return "Requests must be a whole number from 1 to 1,000,000";
      if (!intIn(a.windowSecs, 1, 3600)) return "The window must be 1 to 3600 seconds";
      if (a.key === "header" && !TOKEN.test(a.header ?? "")) return "Rate limiting by header needs a valid header name";
      return null;
    case "block":
      if (!intIn(a.status, 400, 599)) return "A block status must be between 400 and 599";
      if (a.body && byteLen(a.body) > MAX_BODY_BYTES) return `The response body can be at most ${MAX_BODY_BYTES} bytes`;
      return null;
    case "allow":
      return null;
    case "redirect":
      if (!(REDIRECT_STATUSES as readonly number[]).includes(a.status)) return "A redirect status must be 301, 302, 307 or 308";
      if (!a.target.startsWith("/") && !a.target.startsWith("https://")) return "The redirect target must start with / or https://";
      return null;
    case "headers": {
      const set = Object.keys(a.set ?? {});
      const remove = a.remove ?? [];
      if (set.length === 0 && remove.length === 0) return "Set or remove at least one header";
      const bad = [...set, ...remove].find((n) => !TOKEN.test(n));
      return bad ? `"${bad}" isn't a valid header name` : null;
    }
    case "cache":
      return intIn(a.ttlSecs, 1, MAX_CACHE_TTL_SECS) ? null : "The cache time must be 1 to 604800 seconds";
  }
}

/** Problems of one rule, in reading order (empty = valid). */
export function ruleErrors(r: EdgeRule): string[] {
  const out: string[] = [];
  if (r.name.length > MAX_RULE_NAME) out.push(`The name can be at most ${MAX_RULE_NAME} characters`);
  if (r.when.length > MAX_OR_GROUPS) out.push(`At most ${MAX_OR_GROUPS} "or" groups`);
  for (const g of r.when) {
    if (g.length === 0) out.push("Remove the empty condition group");
    else if (g.length > MAX_AND_CONDITIONS) out.push(`At most ${MAX_AND_CONDITIONS} conditions in a group`);
    for (const c of g) {
      const e = conditionError(c);
      if (e && !out.includes(e)) out.push(e);
    }
  }
  const a = actionError(r.then);
  if (a) out.push(a);
  return out;
}

export interface RulesCheck {
  /** Set when the list as a whole is invalid. */
  general: string | null;
  /** Per rule, same order as the input. */
  rules: string[][];
  valid: boolean;
}

export function validateRules(rules: EdgeRule[]): RulesCheck {
  const general = rules.length > MAX_EDGE_RULES ? `At most ${MAX_EDGE_RULES} rules` : null;
  const per = rules.map(ruleErrors);
  return { general, rules: per, valid: !general && per.every((e) => e.length === 0) };
}

// ---- normalising ---------------------------------------------------------------------------

function cleanCondition(c: EdgeCondition): EdgeCondition {
  const negate = c.negate ? { negate: true as const } : {};
  switch (c.field) {
    case "path":
      return { field: "path", op: c.op, value: c.value.trim(), ...negate };
    case "method":
      return { field: "method", values: c.values.map((m) => m.toUpperCase()), ...negate };
    case "header":
      return c.op === "exists"
        ? { field: "header", name: c.name.trim(), op: "exists", ...negate }
        : { field: "header", name: c.name.trim(), op: c.op, value: c.value ?? "", ...negate };
    case "ip":
      return { field: "ip", cidrs: c.cidrs.map((x) => x.trim()), ...negate };
    case "country":
      return { field: "country", values: c.values.map((x) => x.trim().toUpperCase()), ...negate };
  }
}

function cleanAction(a: EdgeAction): EdgeAction {
  switch (a.type) {
    case "rateLimit":
      return a.key === "header"
        ? { type: "rateLimit", requests: a.requests, windowSecs: a.windowSecs, key: "header", header: (a.header ?? "").trim() }
        : { type: "rateLimit", requests: a.requests, windowSecs: a.windowSecs, key: "ip" };
    case "block":
      return a.body ? { type: "block", status: a.status, body: a.body } : { type: "block", status: a.status };
    case "redirect":
      return { type: "redirect", status: a.status, target: a.target.trim() };
    case "headers": {
      const set = Object.fromEntries(Object.entries(a.set ?? {}).map(([k, v]) => [k.trim(), v]));
      const remove = (a.remove ?? []).map((n) => n.trim()).filter(Boolean);
      return {
        type: "headers",
        ...(Object.keys(set).length ? { set } : {}),
        ...(remove.length ? { remove } : {}),
      };
    }
    default:
      return a;
  }
}

/** The rules as the server expects them: trimmed, upper-cased methods and countries, no unused fields. */
export function cleanRules(rules: EdgeRule[]): EdgeRule[] {
  return rules.map((r) => ({
    name: r.name.trim(),
    enabled: r.enabled,
    when: r.when.map((g) => g.map(cleanCondition)),
    then: cleanAction(r.then),
  }));
}

/** The config with `edgeRules` replaced; the key is dropped when there are none. */
export function withEdgeRules(config: ServiceConfig, rules: EdgeRule[]): ServiceConfig {
  const { edgeRules: _old, ...rest } = config;
  return rules.length ? { ...rest, edgeRules: cleanRules(rules) } : rest;
}

// ---- JSON editor ---------------------------------------------------------------------------

export const rulesToJson = (rules: EdgeRule[]): string => JSON.stringify(cleanRules(rules), null, 2);

export type ParsedRules = { ok: true; rules: EdgeRule[] } | { ok: false; error: string };

const isObj = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);

/** Parses the JSON editor's text: syntax errors and the wrong overall shape are reported here;
 *  field-level problems are left to `validateRules`. */
export function parseRulesJson(text: string): ParsedRules {
  let v: unknown;
  try {
    v = JSON.parse(text.trim() === "" ? "[]" : text);
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : "Invalid JSON" };
  }
  if (!Array.isArray(v)) return { ok: false, error: "Expected an array of rules" };
  for (const [i, r] of v.entries()) {
    if (!isObj(r)) return { ok: false, error: `Rule ${i + 1} must be an object` };
    const then = r.then;
    if (!isObj(then) || !ACTION_TYPES.some((a) => a.value === then.type))
      return { ok: false, error: `Rule ${i + 1} needs a "then" action with a known "type"` };
    if (r.when !== undefined && (!Array.isArray(r.when) || !r.when.every((g) => Array.isArray(g) && g.every(isObj))))
      return { ok: false, error: `Rule ${i + 1}: "when" must be a list of condition lists` };
    for (const g of (r.when as unknown[][] | undefined) ?? [])
      for (const c of g)
        if (!CONDITION_FIELDS.some((f) => f.value === (c as Record<string, unknown>).field))
          return { ok: false, error: `Rule ${i + 1} has a condition with an unknown "field"` };
  }
  const rules = (v as Record<string, unknown>[]).map((r): EdgeRule => ({
    name: typeof r.name === "string" ? r.name : "",
    enabled: r.enabled !== false,
    when: ((r.when as EdgeCondition[][] | undefined) ?? []).map((g) => g.map(fillCondition)),
    then: r.then as unknown as EdgeAction,
  }));
  return { ok: true, rules };
}

/** Gives a hand-written condition the arrays and strings the editor expects. */
function fillCondition(c: EdgeCondition): EdgeCondition {
  const o = c as unknown as Record<string, unknown>;
  const list = (k: string) => (Array.isArray(o[k]) ? (o[k] as unknown[]).map(String) : []);
  const str = (k: string) => (typeof o[k] === "string" ? (o[k] as string) : "");
  const negate = o.negate ? { negate: true as const } : {};
  switch (c.field) {
    case "path":
      return { field: "path", op: c.op ?? "matches", value: str("value"), ...negate };
    case "method":
      return { field: "method", values: list("values"), ...negate };
    case "header":
      return { field: "header", name: str("name"), op: c.op ?? "equals", ...(o.value !== undefined ? { value: str("value") } : {}), ...negate };
    case "ip":
      return { field: "ip", cidrs: list("cidrs"), ...negate };
    case "country":
      return { field: "country", values: list("values"), ...negate };
  }
}

/** Moves the item at `from` to index `to`; out-of-range moves return the list unchanged. */
export function moveItem<T>(list: T[], from: number, to: number): T[] {
  if (from === to || from < 0 || to < 0 || from >= list.length || to >= list.length) return list;
  const next = list.slice();
  const [item] = next.splice(from, 1);
  next.splice(to, 0, item!);
  return next;
}
