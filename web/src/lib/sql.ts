// Pure SQL builders for the database browser. Identifiers are always quoted;
// nothing user-typed is interpolated unquoted except column types and default
// expressions, which are validated.

/** `my "table"` -> `"my ""table"""`. */
export function quoteIdent(name: string): string {
  if (name.length === 0) throw new Error("Name can't be empty");
  if (name.includes("\0")) throw new Error("Name can't contain a NUL character");
  return `"${name.replace(/"/g, '""')}"`;
}

/** `public`, `users` -> `"public"."users"`. */
export function qualifiedName(schema: string, name: string): string {
  return `${quoteIdent(schema)}.${quoteIdent(name)}`;
}

/** Table id sent to the API: bare name in `public`, else `schema.name`. */
export function tableRef(t: { schema: string; name: string }): string {
  return t.schema === "public" ? t.name : `${t.schema}.${t.name}`;
}

/** Quotes a table reference that may be `schema.name` or just `name`. */
export function quoteTableRef(ref: string): string {
  const parts = ref.split(".");
  if (parts.length === 2 && parts[0] && parts[1]) return qualifiedName(parts[0], parts[1]);
  return quoteIdent(ref);
}

export const COLUMN_TYPES = [
  "text",
  "varchar(255)",
  "integer",
  "bigint",
  "smallint",
  "serial",
  "bigserial",
  "numeric",
  "real",
  "double precision",
  "boolean",
  "uuid",
  "date",
  "timestamp",
  "timestamptz",
  "jsonb",
  "json",
  "bytea",
] as const;

const TYPE_RE = /^[a-zA-Z][a-zA-Z0-9_ ]*(\(\d+(,\s*\d+)?\))?(\[\])?$/;

export function isValidType(t: string): boolean {
  return TYPE_RE.test(t.trim());
}

/** Default expressions are raw SQL; block statement separators and comments. */
export function isValidDefault(d: string): boolean {
  return !/;|--|\/\*/.test(d);
}

export interface ColumnDraft {
  name: string;
  type: string;
  nullable: boolean;
  primaryKey?: boolean;
  /** Raw SQL expression, e.g. `now()` or `'pending'`. Empty = no default. */
  defaultExpr?: string;
}

export function columnDefinition(c: ColumnDraft): string {
  const type = c.type.trim();
  if (!isValidType(type)) throw new Error(`Invalid type "${c.type}"`);
  const def = (c.defaultExpr ?? "").trim();
  if (def && !isValidDefault(def)) throw new Error("Invalid default expression");
  let out = `${quoteIdent(c.name)} ${type}`;
  if (def) out += ` DEFAULT ${def}`;
  if (c.primaryKey) out += " PRIMARY KEY";
  else if (!c.nullable) out += " NOT NULL";
  return out;
}

export function createTableSql(name: string, columns: ColumnDraft[]): string {
  if (columns.length === 0) throw new Error("A table needs at least one column");
  const names = new Set<string>();
  for (const c of columns) {
    if (names.has(c.name)) throw new Error(`Duplicate column "${c.name}"`);
    names.add(c.name);
  }
  return `CREATE TABLE ${quoteIdent(name)} (\n  ${columns.map(columnDefinition).join(",\n  ")}\n)`;
}

export function addColumnSql(tableRef: string, c: ColumnDraft): string {
  return `ALTER TABLE ${quoteTableRef(tableRef)} ADD COLUMN ${columnDefinition(c)}`;
}

/** Default text for the SQL box's placeholder. */
export function samplePlaceholder(tableRefs: string[]): string {
  return tableRefs.length ? `SELECT * FROM ${quoteTableRef(tableRefs[0]!)}` : "SELECT 1";
}
