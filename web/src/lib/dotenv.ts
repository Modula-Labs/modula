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
