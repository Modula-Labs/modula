import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { provisionApi, type HetznerStatus } from "@/lib/provision";
import { HetznerIntegration } from "./IntegrationsHetzner";

vi.mock("@/lib/provision", async (orig) => {
  const actual = await orig<typeof import("@/lib/provision")>();
  return {
    ...actual,
    provisionApi: { ...actual.provisionApi, hetzner: vi.fn(), updateCaps: vi.fn(), disconnect: vi.fn() },
  };
});

const status: HetznerStatus = {
  connected: true,
  connectedAt: new Date().toISOString(),
  connectedBy: "Ada",
  caps: { maxServers: 3, maxMonthlyCents: 5_000 },
  usage: { servers: 1, monthlyCents: 452 },
  tokenUrl: "https://console.hetzner.cloud/",
};

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("HetznerIntegration", () => {
  it("keeps showing converted prices and saves limits through the shared route", async () => {
    vi.mocked(provisionApi.hetzner).mockResolvedValue(status);
    vi.mocked(provisionApi.updateCaps).mockResolvedValue(status);
    vi.mocked(provisionApi.disconnect).mockResolvedValue({ ...status, connected: false });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <HetznerIntegration workspaceId="ws1" canEdit />
      </QueryClientProvider>,
    );
    expect(await screen.findByText(/In use: 1 server, ~\$4\.52 per month \(incl\. VAT\)/)).toBeTruthy();
    fireEvent.change(screen.getByLabelText("Monthly limit ($)"), { target: { value: "60.5" } });
    fireEvent.click(screen.getByRole("button", { name: "Save limits" }));
    await waitFor(() =>
      expect(provisionApi.updateCaps).toHaveBeenCalledWith("ws1", "hetzner", { maxServers: 3, maxMonthlyCents: 6_050 }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Disconnect" }));
    await waitFor(() => expect(provisionApi.disconnect).toHaveBeenCalledWith("ws1", "hetzner"));
  });
});
