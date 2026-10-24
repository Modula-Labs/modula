import { describe, expect, it } from "vitest";
import type { ServiceConfig } from "./api";
import {
  codeError,
  compatibilityDateError,
  entryError,
  FUNCTION_TEMPLATES,
  functionTarget,
  indent,
  MAX_CODE_BYTES,
  newFunctionConfig,
  newline,
  WORKERD_COMPATIBILITY_DATE,
} from "./functions";
import { fromFunctionForm, functionFormError, toFunctionForm } from "@/pages/project/FunctionSettings";

describe("function code", () => {
  it("accepts every template", () => {
    for (const t of FUNCTION_TEMPLATES) expect(codeError(t.code), t.id).toBeNull();
  });
  it("needs a default export and stays under 1 MB", () => {
    expect(codeError("  ")).toMatch(/Write/);
    expect(codeError("export const x = 1;")).toMatch(/default/);
    expect(codeError(`export default {};//${"é".repeat(MAX_CODE_BYTES / 2)}`)).toMatch(/1 MB/);
  });
});

describe("compatibility dates", () => {
  const today = new Date("2026-10-25T12:00:00Z");
  it("are real dates up to what the runtime supports", () => {
    expect(compatibilityDateError("2026-10-01", "self", today)).toBeNull();
    expect(compatibilityDateError(WORKERD_COMPATIBILITY_DATE, "self", today)).toBeNull();
    expect(compatibilityDateError("2026-10-20", "self", today)).toMatch(/or earlier/);
    expect(compatibilityDateError("2026-10-20", "cloudflare", today)).toBeNull();
    expect(compatibilityDateError("2026-10-26", "cloudflare", today)).toMatch(/future/);
    expect(compatibilityDateError("2026-02-30", "self", today)).toMatch(/like/);
    expect(compatibilityDateError("26-10-01", "self", today)).toMatch(/like/);
    expect(compatibilityDateError("2020-01-01", "self", today)).toMatch(/2021-11-02/);
  });
});

describe("entry files", () => {
  it("are scripts inside the repository", () => {
    expect(entryError("")).toBeNull();
    expect(entryError("src/index.ts")).toBeNull();
    expect(entryError("../x.ts")).not.toBeNull();
    expect(entryError("-x.ts")).not.toBeNull();
    expect(entryError("main.py")).not.toBeNull();
    expect(entryError("a b.ts")).not.toBeNull();
  });
});

describe("the editor's keys", () => {
  it("Tab inserts two spaces at the cursor", () => {
    expect(indent({ value: "ab", selectionStart: 1, selectionEnd: 1 }, false)).toEqual({
      value: "a  b",
      selectionStart: 3,
      selectionEnd: 3,
    });
  });
  it("Tab and Shift+Tab indent and outdent selected lines", () => {
    const v = "a\nb\nc";
    const ind = indent({ value: v, selectionStart: 0, selectionEnd: 3 }, false);
    expect(ind.value).toBe("  a\n  b\nc");
    expect([ind.selectionStart, ind.selectionEnd]).toEqual([2, 7]);
    const out = indent({ ...ind }, true);
    expect(out.value).toBe(v);
    expect(indent({ value: "    x", selectionStart: 4, selectionEnd: 4 }, true).value).toBe("  x");
  });
  it("Enter keeps the indentation, one more after an opening bracket", () => {
    const v = "  if (x) {";
    expect(newline({ value: v, selectionStart: v.length, selectionEnd: v.length }).value).toBe("  if (x) {\n    ");
    expect(newline({ value: "  a", selectionStart: 3, selectionEnd: 3 }).value).toBe("  a\n  ");
  });
});

describe("function configs", () => {
  it("run on the servers, or on Cloudflare without an output directory", () => {
    const self = newFunctionConfig("export default {}", "self");
    expect(self.source).toEqual({ type: "function", code: "export default {}", compatibilityDate: "2026-10-01" });
    expect(self.hosting).toBeUndefined();
    expect(functionTarget(self)).toBe("self");
    const cf = newFunctionConfig("export default {}", "cloudflare");
    expect(cf.hosting).toEqual({ type: "cloudflare", outputDir: "", spa: false, scriptName: undefined });
    expect(functionTarget(cf)).toBe("cloudflare");
  });

  it("round-trip through the settings form, keeping the rest of the config", () => {
    const base: ServiceConfig = {
      ...newFunctionConfig("export default {}", "self"),
      edgeRules: [{ name: "x", enabled: true, when: [], then: { type: "allow" } }],
      replicas: 2,
    };
    const f = toFunctionForm(base);
    expect(fromFunctionForm(base, f)).toEqual({ ...base, healthcheckPath: undefined });
    // Switching to a repository and to Cloudflare.
    const repo = fromFunctionForm(base, { ...f, fromRepo: true, repo: "https://github.com/acme/fns", entry: "src/index.ts", target: "cloudflare" });
    expect(repo.source).toEqual({
      type: "function",
      repo: "acme/fns",
      branch: "main",
      rootDir: undefined,
      entry: "src/index.ts",
      compatibilityDate: "2026-10-01",
    });
    expect(repo.replicas).toBe(1);
    expect(repo.hosting?.type).toBe("cloudflare");
    expect(repo.edgeRules).toEqual(base.edgeRules);
    expect(functionFormError({ ...f, fromRepo: true, repo: "" })).toMatch(/repository/);
    expect(functionFormError({ ...f, compatibilityDate: "2099-01-01" })).toMatch(/Compatibility/);
    expect(functionFormError(f)).toBeNull();
  });
});
