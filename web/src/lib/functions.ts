// JavaScript functions (`source.type === "function"`): templates, the code
// editor's key handling and the server's rules, mirrored for the forms.

import type { ServiceConfig, ServiceSource } from "./api";

export type FunctionSource = Extract<ServiceSource, { type: "function" }>;
export type FunctionTarget = "self" | "cloudflare";

/** The newest compatibility date the servers' workerd supports (the server's WORKERD_COMPATIBILITY_DATE). */
export const WORKERD_COMPATIBILITY_DATE = "2026-10-14";
/** What a new function is written for. */
export const DEFAULT_COMPATIBILITY_DATE = "2026-10-01";
/** Inline code is at most this many bytes (UTF-8). */
export const MAX_CODE_BYTES = 1024 * 1024;
export const DEFAULT_ENTRY = "index.ts";

export interface FunctionTemplate {
  id: string;
  name: string;
  description: string;
  code: string;
}

export const FUNCTION_TEMPLATES: FunctionTemplate[] = [
  {
    id: "hello",
    name: "Hello world",
    description: "Answers every request with a greeting.",
    code: `export default {
  async fetch(request, env, ctx) {
    const name = new URL(request.url).searchParams.get("name") ?? "world";
    return new Response(\`Hello, \${name}!\`);
  },
};
`,
  },
  {
    id: "json-api",
    name: "JSON API",
    description: "Routes a few paths and answers with JSON.",
    code: `const json = (data, status = 200) =>
  new Response(JSON.stringify(data), {
    status,
    headers: { "content-type": "application/json" },
  });

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return json({ ok: true });
    }
    if (request.method === "GET" && url.pathname === "/time") {
      return json({ now: new Date().toISOString() });
    }
    if (request.method === "POST" && url.pathname === "/echo") {
      return json({ received: await request.json() });
    }
    return json({ error: "not found" }, 404);
  },
};
`,
  },
  {
    id: "webhook",
    name: "Webhook handler",
    description: "Checks a signed webhook (HMAC SHA-256) and logs its event.",
    code: `// Set WEBHOOK_SECRET in Variables. The sender signs the raw body with it
// and puts the hex HMAC in the X-Signature header.
async function verify(secret, body, signature) {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["verify"],
  );
  const bytes = new Uint8Array((signature.match(/../g) ?? []).map((h) => parseInt(h, 16)));
  return crypto.subtle.verify("HMAC", key, bytes, new TextEncoder().encode(body));
}

export default {
  async fetch(request, env) {
    if (request.method !== "POST") {
      return new Response("Send a POST", { status: 405 });
    }
    const body = await request.text();
    const signature = request.headers.get("x-signature") ?? "";
    if (!env.WEBHOOK_SECRET || !(await verify(env.WEBHOOK_SECRET, body, signature))) {
      return new Response("Bad signature", { status: 401 });
    }
    const event = JSON.parse(body);
    console.log("webhook", event.type ?? "event", event.id ?? "");
    return new Response(null, { status: 204 });
  },
};
`,
  },
];

/** The function's code size in bytes, as the server counts it. */
export function codeBytes(code: string): number {
  return new TextEncoder().encode(code).length;
}

/** An error message for inline code, or null. */
export function codeError(code: string): string | null {
  if (!code.trim()) return "Write the function's code.";
  if (codeBytes(code) > MAX_CODE_BYTES) return "At most 1 MB: deploy bigger functions from a repository.";
  if (!/export\s+default\b/.test(code)) return "Export a default object with a fetch handler: export default { fetch(request, env) { … } }.";
  return null;
}

/** An error message for a compatibility date, or null (`cloudflare`: today is the newest). */
export function compatibilityDateError(date: string, target: FunctionTarget, today = new Date()): string | null {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(date) || Number.isNaN(Date.parse(`${date}T00:00:00Z`))) return "Use a date like 2026-10-01.";
  const d = new Date(`${date}T00:00:00Z`);
  if (d.toISOString().slice(0, 10) !== date) return "Use a date like 2026-10-01.";
  if (date < "2021-11-02") return "2021-11-02 or later.";
  const newest = target === "cloudflare" ? today.toISOString().slice(0, 10) : WORKERD_COMPATIBILITY_DATE;
  if (date > newest) return target === "cloudflare" ? "Can't be in the future." : `${WORKERD_COMPATIBILITY_DATE} or earlier on your servers.`;
  return null;
}

/** An error message for a repository function's entry file, or null (empty: the default). */
export function entryError(entry: string): string | null {
  const e = entry.trim();
  if (!e) return null;
  if (e.startsWith("/") || e.startsWith("-") || e.split("/").some((s) => s === ".." || s === ".")) return "A path inside the repository.";
  if (!/^[A-Za-z0-9._/-]+$/.test(e)) return "Letters, digits, dots, dashes, underscores and slashes only.";
  if (!/\.(m?[jt]s|[jt]sx)$/.test(e)) return "A .js, .mjs, .ts, .mts, .jsx or .tsx file.";
  return null;
}

/** Where the function runs: Cloudflare when `hosting` is set. */
export function functionTarget(c: Pick<ServiceConfig, "hosting">): FunctionTarget {
  return c.hosting?.type === "cloudflare" ? "cloudflare" : "self";
}

/** `hosting` for a target (a function on Cloudflare has no output directory). */
export function hostingFor(target: FunctionTarget, scriptName?: string): ServiceConfig["hosting"] {
  return target === "cloudflare" ? { type: "cloudflare", outputDir: "", spa: false, scriptName: scriptName?.trim() || undefined } : undefined;
}

/** A new function service's config. */
export function newFunctionConfig(code: string, target: FunctionTarget, compatibilityDate = DEFAULT_COMPATIBILITY_DATE): ServiceConfig {
  return {
    source: { type: "function", code, compatibilityDate },
    port: 8080,
    healthcheckTimeoutSecs: 300,
    restartPolicy: "on_failure",
    hosting: hostingFor(target),
  };
}

export interface Edit {
  value: string;
  selectionStart: number;
  selectionEnd: number;
}

const INDENT = "  ";

/**
 * The code editor's Tab and Shift+Tab: Tab inserts two spaces at the cursor, or
 * indents every selected line; Shift+Tab outdents the current or selected lines.
 */
export function indent(e: Edit, outdent: boolean): Edit {
  const { value, selectionStart: start, selectionEnd: end } = e;
  const lineStart = value.lastIndexOf("\n", start - 1) + 1;
  if (!outdent && start === end) {
    return { value: value.slice(0, start) + INDENT + value.slice(end), selectionStart: start + 2, selectionEnd: start + 2 };
  }
  // Whole lines from the first selected one to the last (a selection ending at
  // a line start leaves that line alone).
  const lastEnd = end > start && value[end - 1] === "\n" ? end - 1 : end;
  const blockEnd = value.indexOf("\n", lastEnd) === -1 ? value.length : value.indexOf("\n", lastEnd);
  const lines = value.slice(lineStart, blockEnd).split("\n");
  let firstDelta = 0;
  let total = 0;
  const changed = lines.map((l, i) => {
    let next: string;
    if (outdent) {
      const n = l.startsWith(INDENT) ? 2 : l.startsWith(" ") || l.startsWith("\t") ? 1 : 0;
      next = l.slice(n);
    } else {
      next = INDENT + l;
    }
    const d = next.length - l.length;
    if (i === 0) firstDelta = d;
    total += d;
    return next;
  });
  const out = value.slice(0, lineStart) + changed.join("\n") + value.slice(blockEnd);
  return {
    value: out,
    selectionStart: Math.max(lineStart, start + firstDelta),
    selectionEnd: Math.max(lineStart, end + total),
  };
}

/** Enter keeps the current line's indentation (one more level after `{`, `[` or `(`). */
export function newline(e: Edit): Edit {
  const { value, selectionStart: start, selectionEnd: end } = e;
  const lineStart = value.lastIndexOf("\n", start - 1) + 1;
  const current = value.slice(lineStart, start);
  const lead = current.match(/^[ \t]*/)?.[0] ?? "";
  const more = /[{[(]\s*$/.test(current) ? INDENT : "";
  const insert = `\n${lead}${more}`;
  return {
    value: value.slice(0, start) + insert + value.slice(end),
    selectionStart: start + insert.length,
    selectionEnd: start + insert.length,
  };
}
