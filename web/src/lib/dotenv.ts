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

/** Starting text for the raw editor. Values are write-only, so existing
 *  variables show up as bare `NAME=` lines (sealed ones included). */
export function envTemplate(names: string[]): string {
  return names.map((n) => `${n}=`).join("\n");
}

export interface EnvDiff {
  /** Payload for the PATCH `set`. */
  set: Record<string, string>;
  /** Existing names whose line was removed. */
  delete: string[];
  added: string[];
  changed: string[];
}

/** What the raw editor sends. The editor can't know current values, so a line
 *  is a change only when it names a new variable or gives an existing one a
 *  non-empty value; an existing `NAME=` is "leave as is" (that is also why an
 *  existing variable can't be blanked here, use its row). Existing names whose
 *  line is gone are deleted. */
export function diffEnv(existing: string[], parsed: Record<string, string>): EnvDiff {
  const have = new Set(existing);
  const set: Record<string, string> = {};
  const added: string[] = [];
  const changed: string[] = [];
  for (const [name, value] of Object.entries(parsed)) {
    if (!have.has(name)) {
      set[name] = value;
      added.push(name);
    } else if (value !== "") {
      set[name] = value;
      changed.push(name);
    }
  }
  return { set, delete: existing.filter((n) => !(n in parsed)), added, changed };
}
