import { describe, expect, it } from "vitest";
import { safeHttpUrl } from "./url";

describe("safeHttpUrl", () => {
  it("keeps http(s) URLs", () => {
    expect(safeHttpUrl("https://github.com/apps/x")).toBe("https://github.com/apps/x");
    expect(safeHttpUrl("http://example.com/")).toBe("http://example.com/");
    expect(safeHttpUrl("http://example.com/", true)).toBeUndefined();
  });
  it("drops scripts, data, credentials and junk", () => {
    for (const bad of [
      "javascript:alert(1)",
      "JaVaScRiPt:alert(1)",
      " javascript:alert(1)",
      "data:text/html,<script>alert(1)</script>",
      "vbscript:x",
      "https://user:pw@github.com/",
      "/relative",
      "",
      null,
      undefined,
    ]) {
      expect(safeHttpUrl(bad)).toBeUndefined();
    }
  });
});
