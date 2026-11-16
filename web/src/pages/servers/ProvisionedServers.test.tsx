import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { provisionApi, type ProvisionedList, type ProvisionedServer } from "@/lib/provision";
import { ProvisionedServers } from "./ProvisionedServers";

vi.mock("@/lib/provision", async (orig) => {
  const actual = await orig<typeof import("@/lib/provision")>();
  return { ...actual, provisionApi: { ...actual.provisionApi, servers: vi.fn() } };
});

const server = (provider: string, name: string, monthlyPriceCents: number, status: ProvisionedServer["status"] = "ready"): ProvisionedServer => ({
  id: name,
  provider,
  providerId: null,
  name,
  location: { id: `${provider}:x`, label: "x", provider, countryCode: "DE", city: provider === "aws" ? "Dublin" : "Falkenstein" },
  serverType: provider === "aws" ? "t3.small" : "cx22",
  monthlyPriceCents,
  status,
  error: null,
  sshEnabled: false,
  ipv4: "203.0.113.7",
  nodeId: null,
  createdAt: "2026-01-01T00:00:00Z",
});
const provider = (kind: string, servers: number, monthlyCents: number, maxMonthlyCents: number) => ({
  kind,
  caps: { maxServers: 5, maxMonthlyCents },
  usage: { servers, monthlyCents },
});

function show(list: ProvisionedList) {
  vi.mocked(provisionApi.servers).mockResolvedValue(list);
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <ProvisionedServers workspaceId="ws1" />
    </QueryClientProvider>,
  );
}
afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("ProvisionedServers", () => {
  it("shows each server's price as its provider bills it, and one usage line per provider", async () => {
    show({
      servers: [server("hetzner", "worker-1", 452), server("aws", "worker-2", 2000)],
      connectedProviders: [provider("hetzner", 1, 452, 5000), provider("aws", 1, 2000, 10_000)],
    });
    expect(await screen.findByText("worker-2")).toBeTruthy();
    expect(screen.getByText(/Falkenstein · Hetzner · 203\.0\.113\.7 · ~\$4\.52\/mo/)).toBeTruthy();
    expect(screen.getByText(/Dublin · AWS · 203\.0\.113\.7 · \$20\.00\/mo/)).toBeTruthy();
    expect(screen.getByText(/Hetzner · 1 of 5 servers · ~\$4\.52 of \$50\.00 per month/)).toBeTruthy();
    expect(screen.getByText(/AWS · 1 of 5 servers · \$20\.00 of \$100\.00 per month/)).toBeTruthy();
  });

  it("stays hidden with no provider and no servers, and keeps servers of a disconnected provider", async () => {
    show({ servers: [], connectedProviders: [] });
    await vi.waitFor(() => expect(provisionApi.servers).toHaveBeenCalled());
    expect(screen.queryByText("Cloud servers")).toBeNull();
    cleanup();

    show({ servers: [server("aws", "old", 2000)], connectedProviders: [] });
    expect(await screen.findByText("old")).toBeTruthy();
  });

  it("labels a resize in progress", async () => {
    show({ servers: [server("aws", "w", 2000, "resizing")], connectedProviders: [provider("aws", 1, 2000, 10_000)] });
    expect(await screen.findByText("Resizing")).toBeTruthy();
  });

  it("says what deleting does at each provider", async () => {
    show({
      servers: [server("hetzner", "worker-1", 452), server("aws", "worker-2", 2000)],
      connectedProviders: [provider("hetzner", 1, 452, 5000), provider("aws", 1, 2000, 10_000)],
    });
    fireEvent.click(await screen.findByRole("button", { name: "Delete worker-2" }));
    expect(await screen.findByText(/terminated at AWS.*Elastic IP is released/)).toBeTruthy();
    cleanup();

    show({ servers: [server("hetzner", "worker-1", 452)], connectedProviders: [provider("hetzner", 1, 452, 5000)] });
    fireEvent.click(await screen.findByRole("button", { name: "Delete worker-1" }));
    expect(await screen.findByText(/deleted at Hetzner with everything on its disk/)).toBeTruthy();
  });
});
