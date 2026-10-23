import { describe, expect, it } from "vitest";
import { parseDotenv } from "./dotenv";

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
