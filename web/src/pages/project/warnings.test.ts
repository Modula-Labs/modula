import { describe, expect, it } from "vitest";
import type { EnvWarning } from "@/lib/api";
import { warningTarget, warningsLabel, worstSeverity } from "./warnings";

const base: EnvWarning = {
  kind: "main_server",
  severity: "warning",
  serviceId: "svc",
  serviceInstanceId: "inst",
  title: "api runs on the main server",
  detail: "",
  docsUrl: "https://usemodula.org/docs/deploy/servers/#keep-apps-off-the-main-server",
  action: { type: "moveServer", nodeId: "n2" },
};

describe("warnings", () => {
  it("leads each fix to the right place", () => {
    expect(warningTarget(base)).toEqual({
      kind: "service",
      serviceId: "svc",
      tab: "settings",
      focus: { instanceId: "inst", target: "server" },
    });
    expect(warningTarget({ ...base, kind: "deploy_failed", action: { type: "openDeployment", deploymentId: "d1" } })).toEqual({
      kind: "service",
      serviceId: "svc",
      tab: "deployments",
      deploymentId: "d1",
    });
    expect(warningTarget({ ...base, serviceId: null, serviceInstanceId: null, action: { type: "addServer" } })).toEqual({ kind: "servers" });
    expect(warningTarget({ ...base, serviceId: null, action: { type: "openVariables" } })).toEqual({ kind: "sharedVariables" });
  });

  it("labels and ranks", () => {
    expect(warningsLabel(1)).toBe("1 warning");
    expect(warningsLabel(4)).toBe("4 warnings");
    expect(worstSeverity([])).toBeNull();
    expect(worstSeverity([{ ...base, severity: "info" }, { ...base, severity: "error" }])).toBe("error");
  });
});
