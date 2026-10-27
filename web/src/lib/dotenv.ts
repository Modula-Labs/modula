// Parses the raw variable editor (.env syntax). Only what people paste in
// practice: KEY=value, optional `export`, quotes, comments and blank lines.

export interface ParsedEnv {
  vars: Record<string, string>;
  errors: string[];
}

const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

export function parseDotenv(text: string): ParsedEnv {
  const vars: Record<string, string> = {};
  const errors: string[] = [];
  text.split(/\r?\n/).forEach((raw, i) => {
    const line = raw.trim();
    if (!line || line.startsWith("#")) return;
    const body = line.startsWith("export ") ? line.slice(7).trimStart() : line;
    const eq = body.indexOf("=");
    if (eq <= 0) {
      errors.push(`Line ${i + 1}: expected NAME=value`);
      return;
    }
    const name = body.slice(0, eq).trim();
    if (!NAME.test(name)) {
      errors.push(`Line ${i + 1}: "${name.slice(0, 40)}" is not a valid name`);
      return;
    }
    if (name.startsWith("MODULA_")) {
      errors.push(`Line ${i + 1}: names starting with MODULA_ are reserved`);
      return;
    }
    vars[name] = unquote(body.slice(eq + 1).trim());
  });
  return { vars, errors };
}

function unquote(v: string): string {
  if (v.length >= 2 && v.startsWith('"') && v.endsWith('"')) {
    return v.slice(1, -1).replace(/\\n/g, "\n").replace(/\\"/g, '"').replace(/\\\\/g, "\\");
  }
  if (v.length >= 2 && v.startsWith("'") && v.endsWith("'")) return v.slice(1, -1);
  // Unquoted values end at an inline comment.
  const hash = v.search(/\s#/);
  return hash === -1 ? v : v.slice(0, hash).trimEnd();
}

/** Starting text for the raw editor: `NAME=value` with the values that could
 *  be revealed, bare `NAME=` for the rest. */
export function envTemplate(names: string[], values: Record<string, string> = {}): string {
  return names.map((n) => `${n}=${n in values ? quote(values[n]!) : ""}`).join("\n");
}

/** A value as one .env line that `parseDotenv` reads back unchanged. */
function quote(v: string): string {
  if (v === "" || /^[^\s"'#\\]*$/.test(v)) return v;
  return `"${v.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/\n/g, "\\n")}"`;
}

export interface EnvDiff {
  /** Payload for the PATCH `set`. */
  set: Record<string, string>;
  /** Existing names whose line was removed. */
  delete: string[];
  added: string[];
  changed: string[];
}

/** What the raw editor sends. For a name whose current value is known
 *  (`values`), a different value is a change. For one that couldn't be
 *  revealed, an empty `NAME=` means "leave as is" and only a typed value
 *  replaces it. New names are added; existing names whose line is gone are
 *  deleted. */
export function diffEnv(existing: string[], parsed: Record<string, string>, values: Record<string, string> = {}): EnvDiff {
  const have = new Set(existing);
  const set: Record<string, string> = {};
  const added: string[] = [];
  const changed: string[] = [];
  for (const [name, value] of Object.entries(parsed)) {
    if (!have.has(name)) {
      set[name] = value;
      added.push(name);
    } else if (name in values ? value !== values[name] : value !== "") {
      set[name] = value;
      changed.push(name);
    }
  }
  return { set, delete: existing.filter((n) => !(n in parsed)), added, changed };
}
