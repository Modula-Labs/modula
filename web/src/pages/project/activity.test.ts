import { describe, expect, it } from "vitest";
import type { ActivityChange } from "@/lib/api";
import { describeChange, groupTitle, initials } from "./activity";

const change = (p: Partial<ActivityChange>): ActivityChange => ({
  kind: "variable",
  op: "added",
  key: null,
  status: null,
  deploymentId: null,
  message: null,
  at: "2026-10-22T12:00:00Z",
  ...p,
});

describe("describeChange", () => {
  it("describes variable changes", () => {
    expect(describeChange(change({ key: "PORT" }))).toBe("Added variable PORT");
    expect(describeChange(change({ op: "updated", key: "PORT" }))).toBe("Updated variable PORT");
    expect(describeChange(change({ op: "removed", key: "X" }))).toBe("Removed variable X");
  });
  it("humanizes config fields", () => {
    expect(describeChange(change({ kind: "config", op: "updated", key: "startCommand" }))).toBe("Changed start command");
  });
  it("describes deployments with the first line of the commit message", () => {
    const c = change({ kind: "deployment", op: "status", status: "failed", message: "fix build\n\nlong body" });
    expect(describeChange(c)).toBe("Deployment failed (fix build)");
    expect(describeChange(change({ kind: "deployment", op: "status", status: "active" }))).toBe("Deployment succeeded");
  });
});

describe("groupTitle / initials", () => {
  it("pluralizes", () => {
    expect(groupTitle({ serviceName: "api", changes: [change({})] })).toBe("1 change in api");
    expect(groupTitle({ serviceName: null, changes: [change({}), change({})] })).toBe("2 changes in project");
  });
  it("takes initials", () => {
    expect(initials("Ada Lovelace")).toBe("AL");
    expect(initials("ada")).toBe("A");
  });
});
