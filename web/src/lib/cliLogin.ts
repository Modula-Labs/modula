/** `modula login` approvals: the code the CLI shows, as `XXXX-XXXX`, or
 *  undefined when `raw` can't be one (8 consonants; case, dash and spaces
 *  are forgiven). Also guards the `?code=` / `?cli=` search params. */
export function cliCode(raw: unknown): string | undefined {
  if (typeof raw !== "string" || raw.length > 16) return undefined;
  const code = raw.replace(/[\s-]/g, "").toUpperCase();
  return /^[BCDFGHJKLMNPQRSTVWXZ]{8}$/.test(code) ? `${code.slice(0, 4)}-${code.slice(4)}` : undefined;
}
