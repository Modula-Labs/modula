import { describe, expect, it } from "vitest";
import { parseSection } from "./sections";

describe("parseSection", () => {
  it("keeps a known section", () => expect(parseSection("webhooks")).toBe("webhooks"));
  it("defaults to general", () => {
    expect(parseSection(undefined)).toBe("general");
    expect(parseSection("nope")).toBe("general");
  });
});
