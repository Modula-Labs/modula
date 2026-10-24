import { describe, expect, it } from "vitest";
import { defaultScriptName, isBuildOnlyVariable, validateOutputDir, validateScriptName, workersDevHost } from "./hosting";

describe("defaultScriptName", () => {
  it.each([
    ["web", "production", "web-production"],
    ["My Site", "Staging 2", "my-site-staging-2"],
    ["api__v2", "pr/#12", "api-v2-pr-12"],
    ["-web-", "prod", "web-prod"],
  ])("%s / %s → %s", (s, e, want) => expect(defaultScriptName(s, e)).toBe(want));
});

describe("validateScriptName", () => {
  it("accepts empty and valid names", () => {
    expect(validateScriptName("")).toBeNull();
    expect(validateScriptName("web-1")).toBeNull();
    expect(validateScriptName("a".repeat(63))).toBeNull();
  });
  it.each(["Web", "web_1", "-web", "web-", "a".repeat(64), "we b"])("rejects %s", (n) => expect(validateScriptName(n)).not.toBeNull());
});

describe("validateOutputDir", () => {
  it("requires a relative path inside the root", () => {
    expect(validateOutputDir("dist")).toBeNull();
    expect(validateOutputDir("apps/web/dist")).toBeNull();
    expect(validateOutputDir(" ")).not.toBeNull();
    expect(validateOutputDir("/dist")).not.toBeNull();
    expect(validateOutputDir("../dist")).not.toBeNull();
  });
});

describe("isBuildOnlyVariable", () => {
  it("matches framework public prefixes only", () => {
    expect(isBuildOnlyVariable("VITE_API_URL")).toBe(true);
    expect(isBuildOnlyVariable("NEXT_PUBLIC_KEY")).toBe(true);
    expect(isBuildOnlyVariable("DATABASE_URL")).toBe(false);
    expect(isBuildOnlyVariable("MY_VITE_X")).toBe(false);
  });
});

describe("workersDevHost", () => {
  it("fills in a placeholder name", () => {
    expect(workersDevHost("web")).toBe("web.<your-subdomain>.workers.dev");
    expect(workersDevHost("")).toBe("<name>.<your-subdomain>.workers.dev");
  });
});
