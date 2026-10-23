import { describe, expect, it } from "vitest";
import { diffEnv, envTemplate, parseDotenv } from "./dotenv";

describe("parseDotenv", () => {
  it("reads the common forms", () => {
    const { vars, errors } = parseDotenv(
      ['# comment', "", "A=1", "export B = two ", 'C="x\\ny"', "D='#raw'", "E=v # note", "F="].join("\n"),
    );
    expect(errors).toEqual([]);
    expect(vars).toEqual({ A: "1", B: "two", C: "x\ny", D: "#raw", E: "v", F: "" });
  });

  it("reports bad lines with numbers", () => {
    const { vars, errors } = parseDotenv("OK=1\nnope\n1BAD=x\nMODULA_X=1");
    expect(vars).toEqual({ OK: "1" });
    expect(errors).toHaveLength(3);
    expect(errors[0]).toContain("Line 2");
  });
});

describe("raw editor diff", () => {
  it("round-trips the template as no change", () => {
    const names = ["A", "B"];
    const d = diffEnv(names, parseDotenv(envTemplate(names)).vars);
    expect(d).toEqual({ set: {}, delete: [], added: [], changed: [] });
  });

  it("sends only new and re-valued names, and deletes removed lines", () => {
    const d = diffEnv(["A", "B", "C"], parseDotenv("A=\nB=new\nD=1\nE=").vars);
    expect(d.set).toEqual({ B: "new", D: "1", E: "" });
    expect(d.added).toEqual(["D", "E"]);
    expect(d.changed).toEqual(["B"]);
    expect(d.delete).toEqual(["C"]);
  });

  it("keeps references verbatim", () => {
    expect(parseDotenv("U=${{postgres.DATABASE_URL}}").vars.U).toBe("${{postgres.DATABASE_URL}}");
  });
});
