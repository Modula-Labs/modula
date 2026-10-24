import { describe, expect, it } from "vitest";
import type { Member } from "./api";
import { expiresIn, inviteToken, isLastAdmin, roleLabel } from "./members";

const m = (id: string, role: Member["role"]): Member => ({
  id,
  userId: id,
  name: id,
  email: `${id}@example.com`,
  role,
  joinedAt: "2026-10-01T00:00:00Z",
  twoFactor: false,
});

describe("members", () => {
  it("finds the last admin", () => {
    const a = m("a", "admin");
    const b = m("b", "member");
    expect(isLastAdmin([a, b], a)).toBe(true);
    expect(isLastAdmin([a, b], b)).toBe(false);
    expect(isLastAdmin([a, b, m("c", "admin")], a)).toBe(false);
  });
  it("formats invite expiry", () => {
    const now = Date.parse("2026-10-22T12:00:00Z");
    expect(expiresIn("2026-10-29T12:00:00Z", now)).toBe("in 7 days");
    expect(expiresIn("2026-10-22T15:00:00Z", now)).toBe("in 3 hours");
    expect(expiresIn("2026-10-22T12:10:00Z", now)).toBe("in 10 min");
    expect(expiresIn("2026-10-22T11:00:00Z", now)).toBe("expired");
  });
  it("accepts only token-shaped invite params", () => {
    expect(inviteToken("mci_abc-DEF_1")).toBe("mci_abc-DEF_1");
    expect(inviteToken("//evil.example")).toBeUndefined();
    expect(inviteToken("https://evil.example/x")).toBeUndefined();
    expect(inviteToken("../projects")).toBeUndefined();
    expect(inviteToken("")).toBeUndefined();
    expect(inviteToken(42)).toBeUndefined();
  });
  it("labels roles", () => {
    expect(roleLabel("deployer")).toBe("Deployer");
  });
});
