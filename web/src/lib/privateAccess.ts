import type { PrivateAccessPhase } from "@/lib/api";

/** Splits a free-form list (one per line, or commas) into emails and email domains (`example.com` or `@example.com`). */
export function parseAllowList(text: string): { emails: string[]; domains: string[]; invalid: string[] } {
  const emails: string[] = [];
  const domains: string[] = [];
  const invalid: string[] = [];
  for (const raw of text.split(/[\s,;]+/)) {
    const v = raw.trim().toLowerCase();
    if (!v) continue;
    if (/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(v)) {
      if (!emails.includes(v)) emails.push(v);
    } else if (/^@?([a-z0-9]([a-z0-9-]*[a-z0-9])?\.)+[a-z]{2,}$/.test(v)) {
      const d = v.replace(/^@/, "");
      if (!domains.includes(d)) domains.push(d);
    } else {
      invalid.push(raw.trim());
    }
  }
  return { emails, domains, invalid };
}

/** The list as the text area shows it. */
export function formatAllowList(emails: string[], domains: string[]): string {
  return [...emails, ...domains.map((d) => `@${d}`)].join("\n");
}

export function isChanging(phase: PrivateAccessPhase | undefined): boolean {
  return phase === "enabling" || phase === "disabling";
}
