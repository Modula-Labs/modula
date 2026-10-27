import { describe, expect, it } from "vitest";
import { filterReferenceOptions, findReferenceQuery, insertReference, referenceOptions } from "./references";

const names = {
  shared: ["API_KEY"],
  services: [
    { serviceId: "a", serviceName: "web", names: ["SECRET"], platform: ["PORT"] },
    { serviceId: "b", serviceName: "postgres", names: ["DATABASE_URL"], platform: ["MODULA_PRIVATE_DOMAIN"] },
  ],
};

describe("findReferenceQuery", () => {
  it("finds the open reference at the caret", () => {
    expect(findReferenceQuery("x=${{post", 9)).toEqual({ start: 2, query: "post" });
    expect(findReferenceQuery("${{}} and ${{a", 14)).toEqual({ start: 10, query: "a" });
  });
  it("ignores closed references and plain text", () => {
    expect(findReferenceQuery("${{shared.A}}", 13)).toBeNull();
    expect(findReferenceQuery("abc", 3)).toBeNull();
  });
});

describe("options", () => {
  const opts = referenceOptions(names, "a");
  it("prefixes other services but not the current one", () => {
    expect(opts.map((o) => o.path)).toEqual([
      "shared.API_KEY",
      "SECRET",
      "PORT",
      "postgres.DATABASE_URL",
      "postgres.MODULA_PRIVATE_DOMAIN",
    ]);
  });
  it("filters case-insensitively, prefix first", () => {
    expect(filterReferenceOptions(opts, "port").map((o) => o.path)).toEqual(["PORT"]);
    expect(filterReferenceOptions(opts, "POST").map((o) => o.path)).toEqual(["postgres.DATABASE_URL", "postgres.MODULA_PRIVATE_DOMAIN"]);
  });
});

describe("insertReference", () => {
  it("completes and moves the caret past it", () => {
    const q = findReferenceQuery("a ${{po", 7)!;
    expect(insertReference("a ${{po", 7, q, "postgres.X")).toEqual({ value: "a ${{postgres.X}}", caret: 17 });
  });
  it("does not double an existing closer", () => {
    const v = "${{po}}tail";
    const q = findReferenceQuery(v, 5)!;
    expect(insertReference(v, 5, q, "web.A")).toEqual({ value: "${{web.A}}tail", caret: 10 });
  });
});
