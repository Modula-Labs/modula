import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "@/lib/api";
import { alertsApi, browserTimezone, type Channel, type TrafficReport } from "@/lib/alerts";
import { NotificationsSection } from "./Notifications";

vi.mock("@/lib/alerts", async (orig) => {
  const actual = await orig<typeof import("@/lib/alerts")>();
  return {
    ...actual,
    alertsApi: {
      channels: vi.fn(),
      createChannel: vi.fn(),
      deleteChannel: vi.fn(),
      testChannel: vi.fn(),
      rules: vi.fn(),
      projectRules: vi.fn(),
      createRule: vi.fn(),
      updateRule: vi.fn(),
      deleteRule: vi.fn(),
      history: vi.fn(),
      trafficReport: vi.fn(),
      setTrafficReport: vi.fn(),
      sendTrafficReport: vi.fn(),
    },
  };
});

const channel = (over: Partial<Channel>): Channel => ({
  id: "c1",
  kind: "email",
  name: "Me",
  host: "m•••@gmail.com",
  createdAt: "2026-11-01T00:00:00Z",
  lastStatus: null,
  lastError: null,
  lastAt: null,
  ...over,
});

const unset: TrafficReport = { enabled: false, timezone: "UTC", channelIds: [], nextAt: null, lastSentAt: null };

function renderSection() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <NotificationsSection workspaceId="ws1" />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  vi.mocked(alertsApi.channels).mockResolvedValue([
    channel({ id: "c1", name: "Me", kind: "email" }),
    channel({ id: "c2", name: "#ops", kind: "slack", host: "hooks.slack.com", createdAt: "2026-11-02T00:00:00Z" }),
  ]);
  vi.mocked(alertsApi.rules).mockResolvedValue([]);
  vi.mocked(alertsApi.history).mockResolvedValue([]);
  vi.mocked(alertsApi.trafficReport).mockResolvedValue(unset);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("email channels", () => {
  it("lists the masked address and adds a channel with the recipient in url", async () => {
    vi.mocked(alertsApi.createChannel).mockResolvedValue(channel({ id: "c3" }));
    renderSection();
    expect((await screen.findAllByText("m•••@gmail.com")).length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole("button", { name: "Add channel" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("Type"), { target: { value: "email" } });
    const address = within(dialog).getByLabelText("Email address") as HTMLInputElement;
    expect(address.placeholder).toBe("you@example.com");
    expect(within(dialog).getByText(/Cloudflare account/)).toBeTruthy();
    const submit = within(dialog).getByRole("button", { name: "Add channel" }) as HTMLButtonElement;
    fireEvent.change(within(dialog).getByLabelText("Name"), { target: { value: "On call" } });
    fireEvent.change(address, { target: { value: "not an address" } });
    expect(submit.disabled).toBe(true);
    fireEvent.change(address, { target: { value: "me@example.com" } });
    expect(submit.disabled).toBe(false);
    fireEvent.click(submit);
    await waitFor(() =>
      expect(alertsApi.createChannel).toHaveBeenCalledWith("ws1", { kind: "email", name: "On call", url: "me@example.com" }),
    );
  });

  it("shows a long email failure as text, not in the badge", async () => {
    const why = "set the sender address in Settings → Integrations → Cloudflare → Email";
    vi.mocked(alertsApi.channels).mockResolvedValue([channel({ lastAt: "2026-11-24T00:00:00Z", lastError: why })]);
    renderSection();
    expect(await screen.findByText(why)).toBeTruthy();
    expect(screen.getAllByText("Failed").length).toBeGreaterThan(0);
  });
});

describe("weekly traffic report", () => {
  it("starts in the browser's timezone, asks for a channel and saves the form", async () => {
    vi.mocked(alertsApi.setTrafficReport).mockResolvedValue({
      enabled: true,
      timezone: browserTimezone(),
      channelIds: ["c1"],
      nextAt: "2026-11-30T09:00:00Z",
      lastSentAt: null,
    });
    renderSection();
    const zone = (await screen.findByLabelText("Timezone")) as HTMLSelectElement;
    expect(zone.value).toBe(browserTimezone());
    const save = screen.getByRole("button", { name: "Save" }) as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    fireEvent.click(screen.getByRole("switch", { name: "Disabled" }));
    expect(save.disabled).toBe(false);
    fireEvent.click(save);
    expect(await screen.findByText("Pick at least one channel.")).toBeTruthy();
    expect(alertsApi.setTrafficReport).not.toHaveBeenCalled();

    fireEvent.click(within(screen.getByRole("form", { name: "Weekly traffic report" })).getByRole("checkbox", { name: /Me/ }));
    fireEvent.click(save);
    await waitFor(() =>
      expect(alertsApi.setTrafficReport).toHaveBeenCalledWith("ws1", { enabled: true, timezone: browserTimezone(), channelIds: ["c1"] }),
    );
    expect(await screen.findByText(/Every Monday at 09:00 .*next:/)).toBeTruthy();
  });

  it("shows the server's error when saving fails", async () => {
    vi.mocked(alertsApi.setTrafficReport).mockRejectedValue(new ApiError(400, "bad_request", "unknown timezone: use an IANA name such as Asia/Kolkata"));
    renderSection();
    await screen.findByLabelText("Timezone");
    fireEvent.click(screen.getByRole("switch", { name: "Disabled" }));
    fireEvent.click(within(screen.getByRole("form", { name: "Weekly traffic report" })).getByRole("checkbox", { name: /#ops/ }));
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText(/unknown timezone/)).toBeTruthy();
  });

  it("sends now to the saved channels and shows each result", async () => {
    vi.mocked(alertsApi.trafficReport).mockResolvedValue({
      enabled: true,
      timezone: "Asia/Kolkata",
      channelIds: ["c2", "c1"],
      nextAt: "2026-11-30T03:30:00Z",
      lastSentAt: null,
    });
    vi.mocked(alertsApi.sendTrafficReport).mockResolvedValue([
      { status: null, error: "connect Cloudflare to send email" },
      { status: 200, error: null },
    ]);
    renderSection();
    expect(await screen.findByText(/Every Monday at 09:00 \(Asia\/Kolkata\) · next:/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Send now" }));
    // Results follow the channel list's order, not the order the ids were saved in.
    expect(await screen.findByText("Me: connect Cloudflare to send email.")).toBeTruthy();
    expect(screen.getByText("#ops: sent.")).toBeTruthy();
    expect(alertsApi.sendTrafficReport).toHaveBeenCalledWith("ws1");
  });
});
