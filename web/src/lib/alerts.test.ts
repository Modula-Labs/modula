import { describe, expect, it } from "vitest";
import {
  CHANNEL_KINDS,
  FALLBACK_TIMEZONES,
  RULE_KINDS,
  browserTimezone,
  channelKindLabel,
  channelStatus,
  describeRule,
  emptyRuleForm,
  formatLocal,
  isChannelTarget,
  isChannelUrl,
  isEmailAddress,
  phaseLabel,
  reportBody,
  reportDeliveries,
  withKind,
  defaultCooldown,
  reportDirty,
  reportDraft,
  reportNeverSet,
  reportSchedule,
  ruleFormBody,
  ruleKindInfo,
  ruleScope,
  sendStatus,
  sendSubject,
  supportedTimezones,
  testResultMessage,
  timezoneOptions,
  validateReportDraft,
  validateRuleForm,
  type RuleForm,
  type TrafficReport,
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

describe("email channels", () => {
  it("is a channel kind that takes an address", () => {
    const email = CHANNEL_KINDS.find((k) => k.value === "email");
    expect(email).toMatchObject({ label: "Email", inputLabel: "Email address", input: "email", placeholder: "you@example.com" });
    expect(email?.hint).toMatch(/Cloudflare/);
    expect(channelKindLabel("email")).toBe("Email");
    expect(CHANNEL_KINDS.filter((k) => k.input === "url").map((k) => k.value)).toEqual(["slack", "discord", "webhook"]);
  });
  it.each([
    ["you@example.com", true],
    [" you@example.com ", true],
    ["a.b+c@mail.example.co", true],
    ["you@localhost", false],
    ["you@@example.com", false],
    ["@example.com", false],
    ["you@", false],
    ["you example@x.com", false],
    ["You <you@example.com>", false],
    ["a@b.c, d@e.f", false],
    ["", false],
  ])("isEmailAddress(%j) -> %s", (a, ok) => expect(isEmailAddress(a)).toBe(ok));
  it("checks the target by kind", () => {
    expect(isChannelTarget("email", "you@example.com")).toBe(true);
    expect(isChannelTarget("email", "https://example.com")).toBe(false);
    expect(isChannelTarget("slack", "https://hooks.slack.com/x")).toBe(true);
    expect(isChannelTarget("webhook", "you@example.com")).toBe(false);
  });
  it("keeps a long failure out of the badge", () => {
    const err = "set the sender address in Settings → Integrations → Cloudflare → Email";
    expect(sendStatus({ status: null, error: err })).toEqual({ label: "Failed", tone: "danger", detail: err });
    expect(channelStatus({ lastStatus: 403, lastError: "the token can't send", lastAt: "t" })).toEqual({
      label: "403",
      tone: "danger",
      detail: "the token can't send",
    });
  });
  it("words the test result for email", () => {
    expect(testResultMessage({ status: 200, error: null }, "email")).toEqual({ ok: true, text: "Test email sent." });
    expect(testResultMessage({ status: null, error: "connect Cloudflare to send email" }, "email")).toEqual({
      ok: false,
      text: "Test failed: connect Cloudflare to send email.",
    });
    // A sentence that ends in a full stop doesn't get a second one; the error beats the status.
    expect(testResultMessage({ status: 403, error: "the token can't send email." }, "email").text).toBe(
      "Test failed: the token can't send email.",
    );
    expect(testResultMessage({ status: 200, error: null }, "slack").text).toBe("Test sent, the receiver answered 200.");
  });
});

describe("new alert kinds", () => {
  it("lists backup completed next to backup failed", () => {
    const kinds = RULE_KINDS.map((k) => k.value);
    expect(kinds.indexOf("backup_completed")).toBe(kinds.indexOf("backup_failed") + 1);
    expect(ruleKindInfo("backup_completed")).toMatchObject({
      label: "Backup completed",
      hint: "A database backup finished: size, duration and destination.",
      threshold: false,
      window: null,
    });
    expect(describeRule({ kind: "backup_completed", threshold: null, windowMinutes: null })).toBe("Backup completed");
    expect(validateRuleForm(form({ kind: "backup_completed" }))).toBeNull();
    expect(ruleFormBody(form({ kind: "backup_completed" }))).not.toHaveProperty("threshold");
  });
  it("labels report sends", () => {
    expect(phaseLabel("report")).toBe("Report");
    expect(phaseLabel("test")).toBe("Test");
    expect(phaseLabel("fired")).toBe("Fired");
    expect(sendSubject({ ruleName: "", phase: "report" })).toBe("Weekly traffic report");
    expect(sendSubject({ ruleName: "", phase: "test" })).toBe("Test message");
    expect(sendSubject({ ruleName: "CPU", phase: "fired" })).toBe("CPU");
  });
});

const report = (over: Partial<TrafficReport> = {}): TrafficReport => ({
  enabled: false,
  timezone: "UTC",
  channelIds: [],
  nextAt: null,
  lastSentAt: null,
  ...over,
});

describe("weekly traffic report form", () => {
  it("starts a report nobody saved yet in the browser's timezone", () => {
    expect(reportNeverSet(report())).toBe(true);
    expect(reportDraft(report(), "Asia/Kolkata")).toEqual({ enabled: false, timezone: "Asia/Kolkata", channelIds: [] });
    expect(reportDirty(reportDraft(report(), "Asia/Kolkata"), report(), "Asia/Kolkata")).toBe(false);
  });
  it("keeps the saved timezone, UTC included, once the report was saved", () => {
    const saved = report({ timezone: "UTC", nextAt: "2026-11-30T09:00:00Z", channelIds: ["c1"] });
    expect(reportNeverSet(saved)).toBe(false);
    expect(reportDraft(saved, "Asia/Kolkata").timezone).toBe("UTC");
    expect(reportNeverSet(report({ lastSentAt: "2026-11-23T09:00:00Z" }))).toBe(false);
  });
  it("is dirty when something changed, whatever the order of the channels", () => {
    const saved = report({ enabled: true, timezone: "Europe/Paris", channelIds: ["a", "b"], nextAt: "2026-11-30T08:00:00Z" });
    const d = reportDraft(saved, "UTC");
    expect(reportDirty(d, saved, "UTC")).toBe(false);
    expect(reportDirty({ ...d, channelIds: ["b", "a"] }, saved, "UTC")).toBe(false);
    expect(reportDirty({ ...d, channelIds: ["a"] }, saved, "UTC")).toBe(true);
    expect(reportDirty({ ...d, enabled: false }, saved, "UTC")).toBe(true);
    expect(reportDirty({ ...d, timezone: "UTC" }, saved, "UTC")).toBe(true);
  });
  it("needs a channel only to turn the report on", () => {
    expect(validateReportDraft({ enabled: true, timezone: "UTC", channelIds: [] })).toMatch(/channel/);
    expect(validateReportDraft({ enabled: true, timezone: "UTC", channelIds: ["a"] })).toBeNull();
    expect(validateReportDraft({ enabled: false, timezone: "UTC", channelIds: [] })).toBeNull();
  });
  it("drops channels that no longer exist from the request", () => {
    expect(reportBody({ enabled: true, timezone: "Asia/Tokyo", channelIds: ["a", "gone"] }, ["a", "b"])).toEqual({
      enabled: true,
      timezone: "Asia/Tokyo",
      channelIds: ["a"],
    });
  });
});

describe("timezones", () => {
  it("puts UTC first and always includes the current zone", () => {
    expect(timezoneOptions(["Europe/Paris", "Asia/Tokyo", "Asia/Calcutta"], "Asia/Kolkata")).toEqual([
      "UTC",
      "Asia/Calcutta",
      "Asia/Kolkata",
      "Asia/Tokyo",
      "Europe/Paris",
    ]);
    expect(timezoneOptions(["UTC", "Asia/Tokyo"], "UTC")).toEqual(["UTC", "Asia/Tokyo"]);
    expect(timezoneOptions([], "UTC")).toEqual(["UTC"]);
  });
  it("reads the browser's list and zone, with fallbacks", () => {
    const zones = supportedTimezones();
    expect(zones.length).toBeGreaterThan(0);
    expect(zones).toContain("Europe/London");
    expect(typeof browserTimezone()).toBe("string");
    expect(FALLBACK_TIMEZONES[0]).toBe("UTC");
    expect(new Set(FALLBACK_TIMEZONES).size).toBe(FALLBACK_TIMEZONES.length);
  });
  it("formats an instant in a given zone", () => {
    // 03:30 UTC is 09:00 in India and 04:30 in Paris (winter).
    expect(formatLocal("2026-11-30T03:30:00Z", "en-GB", "Asia/Kolkata")).toMatch(/Mon.*30.*Nov.*09:00/);
    expect(formatLocal("2026-11-30T03:30:00Z", "en-GB", "Europe/Paris")).toMatch(/04:30/);
    expect(formatLocal("not a date")).toBe("not a date");
  });
  it("describes the schedule, with the next run only when given", () => {
    expect(reportSchedule("Asia/Kolkata", null)).toBe("Every Monday at 09:00 (Asia/Kolkata)");
    const withNext = reportSchedule("Asia/Kolkata", "2026-11-30T03:30:00Z", "en-GB", "Asia/Kolkata");
    expect(withNext).toMatch(/^Every Monday at 09:00 \(Asia\/Kolkata\) · next: Mon.*09:00/);
  });
});

describe("reportDeliveries", () => {
  const channels = [
    { id: "a", name: "#ops" },
    { id: "b", name: "Me" },
    { id: "c", name: "Other" },
  ];
  it("names each result after the saved channels, in order", () => {
    expect(
      reportDeliveries(channels, ["c", "a"], [
        { status: 204, error: null },
        { status: null, error: "connect Cloudflare to send email" },
      ]),
    ).toEqual([
      { name: "#ops", ok: true, text: "#ops: sent." },
      { name: "Other", ok: false, text: "Other: connect Cloudflare to send email." },
    ]);
  });
  it("explains a failure without an error text", () => {
    expect(reportDeliveries(channels, ["a"], [{ status: 500, error: null }])[0]?.text).toBe("#ops: the receiver answered 500.");
    expect(reportDeliveries(channels, ["a"], [{ status: null, error: null }])[0]?.text).toBe("#ops: no response.");
  });
  it("numbers the lines when the counts don't match", () => {
    expect(reportDeliveries(channels, ["a", "gone"], [{ status: 200, error: null }, { status: 200, error: null }]).map((d) => d.name)).toEqual([
      "Channel 1",
      "Channel 2",
    ]);
  });
});

describe("withKind", () => {
  it("drops the cooldown for finished backups unless it was changed", () => {
    expect(defaultCooldown("backup_completed")).toBe(0);
    const f = emptyRuleForm();
    expect(withKind(f, "backup_completed")).toEqual({ kind: "backup_completed", cooldownMinutes: "0" });
    expect(withKind({ ...f, kind: "backup_completed", cooldownMinutes: "0" }, "cpu").cooldownMinutes).toBe(String(defaultCooldown("cpu")));
    expect(withKind({ ...f, cooldownMinutes: "5" }, "backup_completed")).toEqual({ kind: "backup_completed" });
  });
});
