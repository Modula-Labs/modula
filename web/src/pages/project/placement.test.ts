import { describe, expect, it } from "vitest";
import { placeGroup } from "./placement";

describe("placeGroup", () => {
  const app = [{ id: "web", dependsOn: ["db", "cache"] }, { id: "db" }, { id: "cache" }];

  it("keeps the group together, right of the existing cards", () => {
    const p = placeGroup(app, [{ x: 0, y: 0 }, { x: 340, y: 160 }], null);
    expect(p.web).toEqual({ x: 688, y: 96 });
    // Dependencies one column further, stacked.
    expect(p.db!.x).toBe(p.web!.x + 336);
    expect(p.cache!.x).toBe(p.db!.x);
    expect(p.db!.y).not.toBe(p.cache!.y);
    expect(Math.min(p.db!.y, p.cache!.y)).toBe(0);
  });

  it("starts at the anchor when one is given", () => {
    const p = placeGroup(app, [{ x: 0, y: 0 }], { x: 1000, y: 500 });
    expect(Math.min(...Object.values(p).map((q) => q.x))).toBe(864);
    expect(Math.min(...Object.values(p).map((q) => q.y))).toBe(432);
  });

  it("snaps to the grid and handles an empty canvas", () => {
    expect(placeGroup([{ id: "a" }], [], null)).toEqual({ a: { x: 0, y: 0 } });
    expect(placeGroup([], [], null)).toEqual({});
  });
});
