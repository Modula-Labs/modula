import { describe, expect, it } from "vitest";
import { cliCode } from "./cliLogin";

describe("cliCode", () => {
  it("normalizes what people type", () => {
    expect(cliCode("BCDF-GHJK")).toBe("BCDF-GHJK");
    expect(cliCode("bcdfghjk")).toBe("BCDF-GHJK");
    expect(cliCode(" bcdf ghjk ")).toBe("BCDF-GHJK");
  });
  it("rejects anything that can't be a code", () => {
    expect(cliCode("ABCD-EFGH")).toBeUndefined();
    expect(cliCode("BCDF-GHJ")).toBeUndefined();
    expect(cliCode("//evil.example")).toBeUndefined();
    expect(cliCode(42)).toBeUndefined();
    expect(cliCode(undefined)).toBeUndefined();
  });
});
