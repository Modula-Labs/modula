import { describe, expect, it } from "vitest";
import { flagEmoji } from "./flags";

describe("flagEmoji", () => {
  it("maps country codes to flags", () => {
    expect(flagEmoji("DE")).toBe("🇩🇪");
    expect(flagEmoji("us")).toBe("🇺🇸");
  });
  it("ignores invalid input", () => {
    expect(flagEmoji(null)).toBe("");
    expect(flagEmoji("DEU")).toBe("");
  });
});
