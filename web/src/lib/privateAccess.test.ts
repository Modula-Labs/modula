import { describe, expect, it } from "vitest";
import { formatAllowList, isChanging, parseAllowList } from "./privateAccess";

describe("private dashboard allow list", () => {
  it("splits emails and domains", () => {
    const r = parseAllowList("Ana@Example.com, @acme.io\nbob@acme.io  acme.io nope@");
    expect(r.emails).toEqual(["ana@example.com", "bob@acme.io"]);
    expect(r.domains).toEqual(["acme.io"]);
    expect(r.invalid).toEqual(["nope@"]);
  });
  it("round-trips through the text area", () => {
    const text = formatAllowList(["a@x.com"], ["x.com"]);
    expect(text).toBe("a@x.com\n@x.com");
    expect(parseAllowList(text)).toEqual({ emails: ["a@x.com"], domains: ["x.com"], invalid: [] });
  });
  it("knows when a change is running", () => {
    expect(isChanging("enabling")).toBe(true);
    expect(isChanging("on")).toBe(false);
  });
});
