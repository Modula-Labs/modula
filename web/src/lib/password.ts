/** Client-side hint only; the server enforces the rules (at least 10 characters). */
export function strength(pw: string): { score: 0 | 1 | 2 | 3; label: string } {
  if (pw.length < 10) return { score: 0, label: "At least 10 characters" };
  const classes = [/[a-z]/, /[A-Z]/, /\d/, /[^\w\s]/, /\s/].filter((r) => r.test(pw)).length;
  if (pw.length >= 16 || (pw.length >= 12 && classes >= 3)) return { score: 3, label: "Strong" };
  if (pw.length >= 12 || classes >= 3) return { score: 2, label: "Good" };
  return { score: 1, label: "Fair — longer is better" };
}
