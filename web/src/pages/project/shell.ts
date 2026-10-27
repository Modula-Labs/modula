/** Frames of the exec WebSocket (see crates/server/src/exec.rs): binary
 *  frames from the server start with a channel byte (1 stdout, 2 stderr);
 *  text frames are JSON control messages. */
export type ShellFrame =
  | { kind: "output"; stderr: boolean; bytes: Uint8Array }
  | { kind: "exit"; code: number | null; reason: string };

export function decodeFrame(data: ArrayBuffer | string): ShellFrame | null {
  if (typeof data === "string") {
    try {
      const m = JSON.parse(data) as { type?: string; code?: number | null; reason?: string };
      if (m.type === "exit") return { kind: "exit", code: m.code ?? null, reason: m.reason ?? "" };
    } catch {
      /* not JSON */
    }
    return null;
  }
  const all = new Uint8Array(data);
  if (all.length === 0 || (all[0] !== 1 && all[0] !== 2)) return null;
  return { kind: "output", stderr: all[0] === 2, bytes: all.subarray(1) };
}

export const resizeFrame = (cols: number, rows: number) => JSON.stringify({ type: "resize", cols, rows });

/** How a finished session reads in the terminal. */
export function exitLine(code: number | null, reason: string): string {
  if (code !== null) return `Process exited with code ${code}.`;
  return reason ? `Disconnected: ${reason}.` : "Disconnected.";
}
