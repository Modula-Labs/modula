import { describe, expect, it } from "vitest";
import type { EdgeRule, ServiceConfig } from "./api";
import {
  cleanRules,
  defaultAction,
  defaultCondition,
  describeAction,
  isIpOrCidr,
  moveItem,
  newRule,
  parseList,
  parseRulesJson,
  rulesToJson,
  unsupportedMessage,
  validateRules,
  withEdgeRules,
} from "./edge";

const rule = (over: Partial<EdgeRule> = {}): EdgeRule => ({ ...newRule(), ...over });

describe("isIpOrCidr", () => {
  it("accepts addresses and ranges of both families", () => {
    for (const ok of ["203.0.113.7", "203.0.113.0/24", "2001:db8::/32", "::1", "fe80::1/64", "::ffff:10.0.0.1"])
      expect(isIpOrCidr(ok), ok).toBe(true);
  });
  it("rejects malformed input", () => {
    for (const bad of ["", "300.1.1.1", "1.2.3.4/33", "2001:db8::/129", "1.2.3", "hello", "1:2:3", "1.2.3.4/"])
      expect(isIpOrCidr(bad), bad).toBe(false);
  });
});

describe("validateRules", () => {
  it("accepts a default rule and an empty list", () => {
    expect(validateRules([]).valid).toBe(true);
    expect(validateRules([newRule()]).valid).toBe(true);
  });

  it("reports problems per rule", () => {
    const r = validateRules([
      rule(),
      rule({ when: [[{ field: "path", op: "prefix", value: "admin" }]], then: { type: "block", status: 200 } }),
    ]);
    expect(r.valid).toBe(false);
    expect(r.rules[0]).toEqual([]);
    expect(r.rules[1]).toEqual(["A path condition's value must start with /", "A block status must be between 400 and 599"]);
  });

  it("checks conditions by field", () => {
    const bad = rule({
      when: [
        [
          { field: "method", values: [] },
          { field: "header", name: "bad name", op: "exists" },
          { field: "header", name: "X-Key", op: "equals" },
          { field: "ip", cidrs: ["10.0.0.0/33"] },
          { field: "country", values: ["USA"] },
        ],
      ],
    });
    expect(validateRules([bad]).rules[0]).toHaveLength(5);
  });

  it("checks action bounds", () => {
    const a = (then: EdgeRule["then"]) => validateRules([rule({ then })]).valid;
    expect(a({ type: "rateLimit", requests: 0, windowSecs: 60, key: "ip" })).toBe(false);
    expect(a({ type: "rateLimit", requests: 10, windowSecs: 3601, key: "ip" })).toBe(false);
    expect(a({ type: "rateLimit", requests: 10, windowSecs: 60, key: "header" })).toBe(false);
    expect(a({ type: "rateLimit", requests: 10, windowSecs: 60, key: "header", header: "X-Api-Key" })).toBe(true);
    expect(a({ type: "block", status: 403, body: "é".repeat(2049) })).toBe(false);
    expect(a({ type: "redirect", status: 302, target: "http://x.dev" })).toBe(false);
    expect(a({ type: "redirect", status: 301, target: "https://{host}/new{path}" })).toBe(true);
    expect(a({ type: "headers" })).toBe(false);
    expect(a({ type: "headers", set: { "X-Frame-Options": "DENY" } })).toBe(true);
    expect(a({ type: "cache", ttlSecs: 604_801 })).toBe(false);
    expect(a({ type: "cache", ttlSecs: 60 })).toBe(true);
  });

  it("enforces the limits on rules, groups and conditions", () => {
    expect(validateRules(Array.from({ length: 11 }, newRule)).general).toBe("At most 10 rules");
    const c = defaultCondition("method");
    const many = rule({ when: Array.from({ length: 6 }, () => [{ ...c, values: ["GET"] }]) });
    expect(validateRules([many]).valid).toBe(false);
    expect(validateRules([rule({ name: "x".repeat(61) })]).valid).toBe(false);
    expect(validateRules([rule({ when: [[]] })]).valid).toBe(false);
  });
});

describe("cleanRules / withEdgeRules", () => {
  it("normalises case and drops unused fields", () => {
    const [r] = cleanRules([
      rule({
        name: " Block bots ",
        when: [[{ field: "country", values: ["de", " us"], negate: false }, { field: "header", name: "X", op: "exists", value: "left over" }]],
        then: { type: "block", status: 403, body: "" },
      }),
    ]);
    expect(r).toEqual({
      name: "Block bots",
      enabled: true,
      when: [[{ field: "country", values: ["DE", "US"] }, { field: "header", name: "X", op: "exists" }]],
      then: { type: "block", status: 403 },
    });
  });

  it("drops the key when there are no rules", () => {
    const base: ServiceConfig = { source: { type: "image", image: "x" }, port: 80, healthcheckTimeoutSecs: 300, restartPolicy: "always", edgeRules: [newRule()] };
    expect("edgeRules" in withEdgeRules(base, [])).toBe(false);
    expect(withEdgeRules(base, [newRule()]).edgeRules).toHaveLength(1);
  });
});

describe("rules JSON", () => {
  it("round-trips", () => {
    const rules = [rule({ name: "Cache", when: [[{ field: "path", op: "prefix", value: "/static" }]], then: { type: "cache", ttlSecs: 3600 } })];
    const parsed = parseRulesJson(rulesToJson(rules));
    expect(parsed).toEqual({ ok: true, rules });
  });
  it("fills in defaults for hand-written rules", () => {
    const p = parseRulesJson('[{"then":{"type":"allow"},"when":[[{"field":"ip","cidrs":["10.0.0.0/8"]}]]}]');
    expect(p).toEqual({
      ok: true,
      rules: [{ name: "", enabled: true, when: [[{ field: "ip", cidrs: ["10.0.0.0/8"] }]], then: { type: "allow" } }],
    });
  });
  it("reports syntax and shape errors", () => {
    expect(parseRulesJson("[{").ok).toBe(false);
    expect(parseRulesJson("{}")).toEqual({ ok: false, error: "Expected an array of rules" });
    expect(parseRulesJson('[{"then":{"type":"nope"}}]').ok).toBe(false);
    expect(parseRulesJson("  ")).toEqual({ ok: true, rules: [] });
  });
});

describe("helpers", () => {
  it("parseList splits on commas and newlines", () => expect(parseList("US, de,,\nFR ")).toEqual(["US", "de", "FR"]));
  it("moveItem reorders and ignores out-of-range moves", () => {
    expect(moveItem(["a", "b", "c"], 0, 2)).toEqual(["b", "c", "a"]);
    const same = ["a", "b"];
    expect(moveItem(same, 0, -1)).toBe(same);
  });
  it("describeAction names the action and its main parameter", () => {
    expect(describeAction(defaultAction("block"))).toBe("Block 403");
    expect(describeAction({ type: "rateLimit", requests: 100, windowSecs: 60, key: "ip" })).toBe("Rate limit 100/1m");
    expect(describeAction({ type: "cache", ttlSecs: 3600 })).toBe("Cache 1h");
  });
  it("unsupportedMessage names the missing module", () => {
    expect(unsupportedMessage(["rateLimit"])).toBe("Rate limit rules aren't applied on one of this service's servers: its edge has no rate-limit module.");
    expect(unsupportedMessage(["rateLimit", "cache"])).toContain("Rate limit and cache rules");
  });
});
