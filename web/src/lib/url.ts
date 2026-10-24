// URLs from API responses that end up in an href or a navigation. The server
// validates the ones it stores, but the browser is the last line: anything
// that isn't plain http(s) (javascript:, data:, …) is dropped.

/** `raw` if it is an absolute http(s) URL without credentials (https only
 *  when `httpsOnly`), else undefined. */
export function safeHttpUrl(raw: string | null | undefined, httpsOnly = false): string | undefined {
  if (!raw) return undefined;
  let u: URL;
  try {
    u = new URL(raw);
  } catch {
    return undefined;
  }
  const ok = u.protocol === "https:" || (!httpsOnly && u.protocol === "http:");
  if (!ok || u.username || u.password) return undefined;
  return u.href;
}
