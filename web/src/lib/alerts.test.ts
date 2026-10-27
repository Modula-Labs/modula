import { describe, expect, it } from "vitest";
import {
  channelStatus,
  describeRule,
  emptyRuleForm,
  isChannelUrl,
  ruleFormBody,
  ruleScope,
  sendStatus,
  testResultMessage,
  validateRuleForm,
  type RuleForm,
} from "./alerts";

const form = (over: Partial<RuleForm>): RuleForm => ({ ...emptyRuleForm(), name: "r", channelIds: ["c1"], ...over });

describe("describeRule", () => {
  it("describes resource rules with threshold and window", () => {
    expect(describeRule({ kind: "cpu", threshold: 80, windowMinutes: 5 })).toBe("CPU above 80% for 5 min");
    expect(describeRule({ kind: "memory", threshold: 90, windowMinutes: 120 })).toBe("Memory above 90% of limit for 2 h");
    expect(describeRule({ kind: "disk", threshold: 85, windowMinutes: null })).toBe("Disk above 85% used");
    expect(describeRule({ kind: "node_offline", threshold: null, windowMinutes: 1440 })).toBe("Server offline for 1 day");
  });
  it("uses the kind label for event rules", () =>
    expect(describeRule({ kind: "build_failed", threshold: null, windowMinutes: null })).toBe("Build failed"));
});

describe("ruleScope", () => {
  it("names the scope", () => {
    expect(ruleScope({ projectId: null, projectName: null, serviceId: null, serviceName: null })).toBe("Whole workspace");
    expect(ruleScope({ projectId: "p", projectName: "Shop", serviceId: null, serviceName: null })).toBe("Shop");
    expect(ruleScope({ projectId: "p", projectName: "Shop", serviceId: "s", serviceName: "api" })).toBe("Shop / api");
  });
});

describe("validateRuleForm", () => {
  it("accepts an event rule", () => expect(validateRuleForm(form({}))).toBeNull());
  it("requires a name and a channel", () => {
    expect(validateRuleForm(form({ name: " " }))).toMatch(/name/);
    expect(validateRuleForm(form({ channelIds: [] }))).toMatch(/channel/);
  });
  it("checks the threshold only for kinds that use it", () => {
    expect(validateRuleForm(form({ kind: "cpu", threshold: "101" }))).toMatch(/Threshold/);
    expect(validateRuleForm(form({ kind: "cpu", threshold: "0" }))).toMatch(/Threshold/);
    expect(validateRuleForm(form({ kind: "cpu", threshold: "80" }))).toBeNull();
    expect(validateRuleForm(form({ kind: "build_failed", threshold: "999" }))).toBeNull();
  });
  it("checks the window range per kind", () => {
    expect(validateRuleForm(form({ kind: "cpu", windowMinutes: "61" }))).toMatch(/Window/);
    expect(validateRuleForm(form({ kind: "node_offline", windowMinutes: "1440" }))).toBeNull();
    expect(validateRuleForm(form({ kind: "node_offline", windowMinutes: "0" }))).toMatch(/Window/);
  });
  it("checks the cooldown and the scope", () => {
    expect(validateRuleForm(form({ cooldownMinutes: "1441" }))).toMatch(/Cooldown/);
    expect(validateRuleForm(form({ cooldownMinutes: "abc" }))).toMatch(/Cooldown/);
    expect(validateRuleForm(form({ cooldownMinutes: "0" }))).toBeNull();
    expect(validateRuleForm(form({ serviceId: "s" }))).toMatch(/project/);
  });
});

describe("ruleFormBody", () => {
  it("drops fields the kind does not use", () =>
    expect(ruleFormBody(form({ kind: "deployment_failed", name: " x " }))).toEqual({
      name: "x",
      kind: "deployment_failed",
      cooldownMinutes: 30,
      channelIds: ["c1"],
    }));
  it("keeps threshold, window and scope", () =>
    expect(ruleFormBody(form({ kind: "memory", projectId: "p", serviceId: "s", threshold: "70", windowMinutes: "10" }))).toMatchObject({
      projectId: "p",
      serviceId: "s",
      threshold: 70,
      windowMinutes: 10,
    }));
});

describe("isChannelUrl", () => {
  it.each([
    ["https://hooks.slack.com/services/x", true],
    ["ftp://x", false],
    ["nope", false],
  ])("%s -> %s", (u, ok) => expect(isChannelUrl(u)).toBe(ok));
});

describe("statuses", () => {
  it("channelStatus", () => {
    expect(channelStatus({ lastStatus: null, lastError: null, lastAt: null })).toEqual({ label: "Never sent", tone: "neutral" });
    expect(channelStatus({ lastStatus: 200, lastError: null, lastAt: "t" })).toEqual({ label: "200 OK", tone: "success" });
    expect(channelStatus({ lastStatus: null, lastError: "timeout", lastAt: "t" })).toEqual({ label: "timeout", tone: "danger" });
  });
  it("sendStatus", () => {
    expect(sendStatus({ status: 204, error: null }).tone).toBe("success");
    expect(sendStatus({ status: 500, error: null })).toEqual({ label: "500", tone: "danger" });
  });
  it("testResultMessage", () => {
    expect(testResultMessage({ status: 200, error: null }).ok).toBe(true);
    expect(testResultMessage({ status: null, error: "dns" })).toEqual({ ok: false, text: "Test failed: dns." });
  });
});
