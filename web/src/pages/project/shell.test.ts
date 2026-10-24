import { describe, expect, it } from "vitest";
import { execUrl } from "@/lib/api";
import { decodeFrame, exitLine, resizeFrame } from "./shell";

describe("shell frames", () => {
  it("splits output by channel and reads the exit message", () => {
    const out = decodeFrame(new Uint8Array([1, 104, 105]).buffer);
    expect(out).toEqual({ kind: "output", stderr: false, bytes: new Uint8Array([104, 105]) });
    const err = decodeFrame(new Uint8Array([2, 33]).buffer);
    expect(err).toMatchObject({ kind: "output", stderr: true });
    expect(decodeFrame(new Uint8Array([9, 1]).buffer)).toBeNull();
    expect(decodeFrame(new ArrayBuffer(0))).toBeNull();
    expect(decodeFrame('{"type":"exit","code":3,"reason":"exited with code 3"}')).toEqual({
      kind: "exit",
      code: 3,
      reason: "exited with code 3",
    });
    expect(decodeFrame('{"type":"exit","code":null,"reason":"idle for 30 minutes"}')).toMatchObject({ code: null });
    expect(decodeFrame("nope")).toBeNull();
  });

  it("builds URLs and control frames", () => {
    expect(execUrl("d1", { cols: 120, rows: 40 }, { protocol: "https:", host: "cloud.test" })).toBe(
      "wss://cloud.test/api/deployments/d1/exec?tty=true&cols=120&rows=40",
    );
    expect(execUrl("d1", { cols: 80, rows: 24 }, { protocol: "http:", host: "localhost:5173" })).toMatch(/^ws:\/\/localhost:5173\//);
    expect(JSON.parse(resizeFrame(100, 30))).toEqual({ type: "resize", cols: 100, rows: 30 });
    expect(exitLine(0, "")).toBe("Process exited with code 0.");
    expect(exitLine(null, "the deployment was removed")).toBe("Disconnected: the deployment was removed.");
  });
});
