import { COLUMN_GAP, layeredLayout } from "./edges";
import { snapPoint } from "./snap";

type Point = { x: number; y: number };

/**
 * Positions for services that appeared together (an app template): laid out
 * among themselves like the default layout, with the group's top-left card
 * at `anchor` (centred on it, as a single new card is), else one column to
 * the right of the existing cards.
 */
export function placeGroup(
  group: { id: string; dependsOn?: string[] }[],
  existing: Point[],
  anchor: Point | null,
  card = { width: 288, height: 144 },
): Record<string, Point> {
  const rel = layeredLayout(group);
  const points = Object.values(rel);
  if (!points.length) return {};
  const minX = Math.min(...points.map((p) => p.x));
  const minY = Math.min(...points.map((p) => p.y));
  const origin = anchor
    ? { x: anchor.x - card.width / 2, y: anchor.y - card.height / 2 }
    : existing.length
      ? { x: Math.max(...existing.map((p) => p.x)) + COLUMN_GAP, y: Math.min(...existing.map((p) => p.y)) }
      : { x: 0, y: 0 };
  return Object.fromEntries(
    Object.entries(rel).map(([id, p]) => [id, snapPoint({ x: origin.x + p.x - minX, y: origin.y + p.y - minY })]),
  );
}
