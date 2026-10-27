import { describe, expect, it } from "vitest";
import { GRID, snap, snapPoint, freeSpot } from "./snap";

describe("snap", () => {
  it("rounds to the nearest grid point", () => {
    expect(snap(0, 16)).toBe(0);
    expect(snap(7, 16)).toBe(0);
    expect(snap(9, 16)).toBe(16);
    expect(snap(-9, 16)).toBe(-16);
    expect(snap(160, 16)).toBe(160);
  });
  it("never returns negative zero", () => {
    expect(Object.is(snap(-3, 16), 0)).toBe(true);
  });
  it("ignores a non-positive grid", () => {
    expect(snap(5.5, 0)).toBe(5.5);
  });
  it("defaults to the canvas grid", () => {
    expect(snap(GRID * 3 + 1)).toBe(GRID * 3);
  });
});

describe("snapPoint", () => {
  it("snaps both axes", () => {
    expect(snapPoint({ x: 21, y: -30 }, 16)).toEqual({ x: 16, y: -32 });
  });
});

describe("freeSpot", () => {
  it("keeps a free spot and moves below cards in the way", () => {
    expect(freeSpot({ x: 0, y: 0 }, [{ x: 400, y: 0 }])).toEqual({ x: 0, y: 0 });
    expect(freeSpot({ x: 0, y: 0 }, [{ x: 16, y: 32 }])).toEqual({ x: 0, y: 192 });
    expect(freeSpot({ x: 0, y: 0 }, [{ x: 0, y: 0 }, { x: 0, y: 160 }])).toEqual({ x: 0, y: 320 });
  });
});
