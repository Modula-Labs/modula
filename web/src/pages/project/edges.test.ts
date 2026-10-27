import { describe, expect, it } from "vitest";
import { dependencyPairs, edgeAnchors, layeredLayout } from "./edges";

const card = (x: number, y: number) => ({ x, y, w: 288, h: 144 });

describe("edgeAnchors", () => {
  it("connects right to left when the dependency is to the right", () => {
    const { source, target } = edgeAnchors(card(0, 0), card(340, 0));
    expect([source.side, target.side]).toEqual(["right", "left"]);
    expect(source).toMatchObject({ x: 288, y: 72 });
    expect(target).toMatchObject({ x: 340, y: 72 });
  });
  it("connects left to right when it is to the left", () => {
    const { source, target } = edgeAnchors(card(340, 0), card(0, 0));
    expect([source.side, target.side]).toEqual(["left", "right"]);
  });
  it("goes vertical when the cards overlap horizontally", () => {
    expect(edgeAnchors(card(0, 0), card(100, 200)).source.side).toBe("bottom");
    expect(edgeAnchors(card(0, 200), card(100, 0)).source.side).toBe("top");
  });
});

describe("dependencyPairs", () => {
  it("drops unknown ids, self references and duplicates", () => {
    expect(dependencyPairs([{ id: "a", dependsOn: ["b", "b", "a", "zz"] }, { id: "b" }])).toEqual([["a", "b"]]);
  });
});

describe("layeredLayout", () => {
  it("puts dependencies in columns to the right of their consumers", () => {
    const pos = layeredLayout([
      { id: "web", dependsOn: ["api"] },
      { id: "api", dependsOn: ["db", "cache"] },
      { id: "db" },
      { id: "cache" },
      { id: "worker", dependsOn: ["db"] },
    ]);
    expect(pos.web?.x).toBe(0);
    expect(pos.worker?.x).toBe(0);
    expect(pos.api?.x).toBe(340);
    expect(pos.db?.x).toBe(680);
    expect(pos.cache?.x).toBe(680);
    expect(pos.db?.y).not.toBe(pos.cache?.y);
  });

  it("survives service-level loops", () => {
    const pos = layeredLayout([
      { id: "a", dependsOn: ["b"] },
      { id: "b", dependsOn: ["a"] },
    ]);
    expect(Object.keys(pos)).toHaveLength(2);
  });
});
