// Helpers for `${{service.NAME}}` references: finding the one being typed,
// listing what it can complete to, and splicing the choice in.
import type { VariableNames } from "./api";

export interface ReferenceQuery {
  /** Index of the opening `${{`. */
  start: number;
  /** Text typed after it, up to the caret. */
  query: string;
}

/** The unfinished `${{…` the caret is inside, if any. */
export function findReferenceQuery(value: string, caret: number): ReferenceQuery | null {
  const before = value.slice(0, caret);
  const start = before.lastIndexOf("${{");
  if (start === -1) return null;
  const query = before.slice(start + 3);
  if (query.includes("}}") || query.includes("\n")) return null;
  return { start, query };
}

export interface ReferenceOption {
  /** Group heading: "Shared", or the service name. */
  group: string;
  /** Shown and matched against, e.g. `postgres.DATABASE_URL`. */
  label: string;
  /** What goes between `${{` and `}}`. */
  path: string;
  platform?: boolean;
}

/** Every referenceable name. The current service's own names are written
 *  without a prefix (`${{NAME}}`), everyone else's with their service name. */
export function referenceOptions(names: VariableNames, selfServiceId?: string): ReferenceOption[] {
  const out: ReferenceOption[] = names.shared.map((n) => ({ group: "Shared", label: `shared.${n}`, path: `shared.${n}` }));
  for (const s of names.services) {
    const own = s.serviceId === selfServiceId;
    const prefix = own ? "" : `${s.serviceName}.`;
    const group = own ? `${s.serviceName} (this service)` : s.serviceName;
    for (const n of s.names) out.push({ group, label: prefix + n, path: prefix + n });
    for (const n of s.platform) out.push({ group, label: prefix + n, path: prefix + n, platform: true });
  }
  return out;
}

export function filterReferenceOptions(options: ReferenceOption[], query: string, limit = 40): ReferenceOption[] {
  const q = query.trim().toLowerCase();
  const hits = q ? options.filter((o) => o.label.toLowerCase().includes(q)) : options;
  // Prefix matches first, then keep the original (shared, services) order.
  const ranked = q ? [...hits].sort((a, b) => Number(b.label.toLowerCase().startsWith(q)) - Number(a.label.toLowerCase().startsWith(q))) : hits;
  return ranked.slice(0, limit);
}

/** Replaces the `${{query` before the caret with `${{path}}`, swallowing a
 *  `}}` already right after the caret so it isn't doubled. */
export function insertReference(value: string, caret: number, q: ReferenceQuery, path: string): { value: string; caret: number } {
  const ref = `\${{${path}}}`;
  const rest = value.slice(caret);
  const tail = rest.startsWith("}}") ? rest.slice(2) : rest;
  return { value: value.slice(0, q.start) + ref + tail, caret: q.start + ref.length };
}
