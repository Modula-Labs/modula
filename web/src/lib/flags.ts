/** ISO 3166 alpha-2 code to its flag emoji; empty for anything else. */
export function flagEmoji(countryCode: string | null | undefined): string {
  if (!countryCode || !/^[a-z]{2}$/i.test(countryCode)) return "";
  return String.fromCodePoint(...[...countryCode.toUpperCase()].map((c) => 0x1f1e6 + c.charCodeAt(0) - 65));
}
