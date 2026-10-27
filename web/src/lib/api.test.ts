import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError, parseError, request, setSudoHandler } from "./api";

const json = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

afterEach(() => {
  vi.restoreAllMocks();
  setSudoHandler(null);
});

describe("api client", () => {
  it("parses server errors", async () => {
    const e = await parseError(json(401, { error: { code: "invalid_credentials", message: "invalid credentials" } }));
    expect(e).toBeInstanceOf(ApiError);
    expect(e.code).toBe("invalid_credentials");
    expect(e.status).toBe(401);
    const plain = await parseError(new Response("boom", { status: 502, statusText: "Bad Gateway" }));
    expect(plain.code).toBe("http_502");
  });

  it("re-authenticates once on sudo_required and retries", async () => {
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValueOnce(json(403, { error: { code: "sudo_required", message: "re-authentication required" } }))
      .mockResolvedValueOnce(json(200, { ok: true }));
    const handler = vi.fn().mockResolvedValue(true);
    setSudoHandler(handler);
    await expect(request("DELETE", "/api/account/totp")).resolves.toEqual({ ok: true });
    expect(handler).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("gives up when the user cancels re-authentication", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(json(403, { error: { code: "sudo_required", message: "x" } }));
    setSudoHandler(vi.fn().mockResolvedValue(false));
    await expect(request("DELETE", "/api/account/totp")).rejects.toMatchObject({ code: "sudo_required" });
  });

  it("sends JSON with same-origin credentials", async () => {
    const fetchMock = vi.spyOn(globalThis, "fetch").mockResolvedValue(json(200, {}));
    await request("POST", "/api/auth/login", { email: "a@b.co", password: "x" });
    const [, init] = fetchMock.mock.calls[0]!;
    expect(init?.credentials).toBe("same-origin");
    expect((init?.headers as Record<string, string>)["content-type"]).toBe("application/json");
  });
});
