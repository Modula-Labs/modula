/** Spacing of the canvas dot grid, in canvas units. Cards snap to it, so it
 *  must be the exact `gap` of the dotted background. Card sizes are multiples. */
export const GRID = 16;

/** Nearest multiple of `grid` (never -0). */
export function snap(value: number, grid: number = GRID): number {
  if (!(grid > 0)) return value;
  return Math.round(value / grid) * grid + 0;
}

export function snapPoint(p: { x: number; y: number }, grid: number = GRID): { x: number; y: number } {
  return { x: snap(p.x, grid), y: snap(p.y, grid) };
}

/** A card's footprint on the canvas, in canvas units. */
export const CARD = { w: 288, h: 144 };

/** `want`, or the first spot below it where a card doesn't overlap any of `taken`. */
export function freeSpot(
  want: { x: number; y: number },
  taken: { x: number; y: number }[],
  gap: number = GRID,
): { x: number; y: number } {
  const p = { ...want };
  for (;;) {
    const blocking = taken.filter(
      (t) => Math.abs(t.x - p.x) < CARD.w + gap && Math.abs(t.y - p.y) < CARD.h + gap,
    );
    if (blocking.length === 0) return p;
    p.y = Math.max(...blocking.map((t) => t.y)) + CARD.h + gap;
  }
}
