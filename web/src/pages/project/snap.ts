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
