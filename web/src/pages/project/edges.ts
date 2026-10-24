// Geometry for the dependency edges on the canvas.

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}
export type Side = "left" | "right" | "top" | "bottom";
export interface Anchor {
  x: number;
  y: number;
  side: Side;
}

const anchor = (r: Rect, side: Side): Anchor => ({
  side,
  x: side === "left" ? r.x : side === "right" ? r.x + r.w : r.x + r.w / 2,
  y: side === "top" ? r.y : side === "bottom" ? r.y + r.h : r.y + r.h / 2,
});

/** Where an edge from `from` to `to` leaves and enters: side to side when the
 *  cards are apart horizontally, otherwise top to bottom. */
export function edgeAnchors(from: Rect, to: Rect): { source: Anchor; target: Anchor } {
  if (to.x >= from.x + from.w) return { source: anchor(from, "right"), target: anchor(to, "left") };
  if (to.x + to.w <= from.x) return { source: anchor(from, "left"), target: anchor(to, "right") };
  return to.y >= from.y
    ? { source: anchor(from, "bottom"), target: anchor(to, "top") }
    : { source: anchor(from, "top"), target: anchor(to, "bottom") };
}

/** Unique `consumer>dependency` pairs between services that are on the canvas. */
export function dependencyPairs(services: { id: string; dependsOn?: string[] }[]): [string, string][] {
  const ids = new Set(services.map((s) => s.id));
  const seen = new Set<string>();
  const out: [string, string][] = [];
  for (const s of services) {
    for (const d of s.dependsOn ?? []) {
      const key = `${s.id}>${d}`;
      if (d === s.id || !ids.has(d) || seen.has(key)) continue;
      seen.add(key);
      out.push([s.id, d]);
    }
  }
  return out;
}

export const COLUMN_GAP = 340;
const ROW_GAP = 180;

/**
 * Default card positions: consumers on the left, the services they depend on
 * in columns to the right (each service one column past its furthest
 * consumer), stacked and centred vertically. Service-level loops (possible
 * even when variables don't loop) are cut at the number of services.
 */
export function layeredLayout(services: { id: string; dependsOn?: string[] }[]): Record<string, { x: number; y: number }> {
  const ids = new Set(services.map((s) => s.id));
  const column = new Map(services.map((s) => [s.id, 0]));
  const col = (id: string) => column.get(id) ?? 0;
  for (let pass = 0; pass < services.length; pass++) {
    let changed = false;
    for (const s of services) {
      const next = col(s.id) + 1;
      for (const d of s.dependsOn ?? []) {
        if (ids.has(d) && d !== s.id && col(d) < next && next < services.length) {
          column.set(d, next);
          changed = true;
        }
      }
    }
    if (!changed) break;
  }
  const byColumn = new Map<number, string[]>();
  for (const s of services) byColumn.set(col(s.id), [...(byColumn.get(col(s.id)) ?? []), s.id]);
  const out: Record<string, { x: number; y: number }> = {};
  for (const [c, members] of byColumn) {
    members.forEach((id, row) => {
      out[id] = { x: c * COLUMN_GAP, y: row * ROW_GAP - ((members.length - 1) * ROW_GAP) / 2 };
    });
  }
  return out;
}
