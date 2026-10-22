// Per-browser conveniences only (theme, last workspace). Storage can be
// unavailable (private mode, blocked site data), so every access is guarded.
export function load(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function save(key: string, value: string) {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    /* ignore */
  }
}
