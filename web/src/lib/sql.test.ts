import { describe, expect, it } from "vitest";
import { addColumnSql, columnDefinition, createTableSql, isValidDefault, isValidType, quoteIdent, quoteTableRef, samplePlaceholder, tableRef } from "./sql";

describe("sql builders", () => {
  it("quotes identifiers", () => {
    expect(quoteIdent("users")).toBe('"users"');
    expect(quoteIdent('we"ird')).toBe('"we""ird"');
    expect(quoteIdent("a; DROP TABLE x")).toBe('"a; DROP TABLE x"');
    expect(() => quoteIdent("")).toThrow();
    expect(() => quoteIdent("a\0b")).toThrow();
  });
  it("handles table refs", () => {
    expect(tableRef({ schema: "public", name: "t" })).toBe("t");
    expect(tableRef({ schema: "app", name: "t" })).toBe("app.t");
    expect(quoteTableRef("app.t")).toBe('"app"."t"');
    expect(quoteTableRef("t")).toBe('"t"');
    expect(samplePlaceholder(["a b"])).toBe('SELECT * FROM "a b"');
    expect(samplePlaceholder([])).toBe("SELECT 1");
  });
  it("validates types and defaults", () => {
    expect(isValidType("varchar(255)")).toBe(true);
    expect(isValidType("numeric(10, 2)")).toBe(true);
    expect(isValidType("double precision")).toBe(true);
    expect(isValidType("text[]")).toBe(true);
    expect(isValidType("text); DROP TABLE x; --")).toBe(false);
    expect(isValidDefault("now()")).toBe(true);
    expect(isValidDefault("1; DROP TABLE x")).toBe(false);
  });
  it("builds column definitions", () => {
    expect(columnDefinition({ name: "id", type: "uuid", nullable: false, primaryKey: true, defaultExpr: "gen_random_uuid()" })).toBe(
      '"id" uuid DEFAULT gen_random_uuid() PRIMARY KEY',
    );
    expect(columnDefinition({ name: "n", type: "text", nullable: false })).toBe('"n" text NOT NULL');
    expect(columnDefinition({ name: "n", type: "text", nullable: true })).toBe('"n" text');
    expect(() => columnDefinition({ name: "n", type: "text;", nullable: true })).toThrow();
  });
  it("builds CREATE TABLE and ADD COLUMN", () => {
    expect(
      createTableSql("my table", [
        { name: "id", type: "serial", nullable: false, primaryKey: true },
        { name: "name", type: "text", nullable: true },
      ]),
    ).toBe('CREATE TABLE "my table" (\n  "id" serial PRIMARY KEY,\n  "name" text\n)');
    expect(() => createTableSql("t", [])).toThrow();
    expect(() => createTableSql("t", [{ name: "a", type: "text", nullable: true }, { name: "a", type: "text", nullable: true }])).toThrow();
    expect(addColumnSql("app.t", { name: "c", type: "integer", nullable: false, defaultExpr: "0" })).toBe(
      'ALTER TABLE "app"."t" ADD COLUMN "c" integer DEFAULT 0 NOT NULL',
    );
  });
});
