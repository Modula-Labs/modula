import { describe, expect, it } from "vitest";
import { GRID, snap, snapPoint } from "./snap";

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
