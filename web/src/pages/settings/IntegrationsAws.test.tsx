import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { provisionApi, type AwsStatus } from "@/lib/provision";
import { AwsIntegration } from "./IntegrationsAws";

vi.mock("@/lib/provision", async (orig) => {
  const actual = await orig<typeof import("@/lib/provision")>();
  return {
    ...actual,
    provisionApi: { ...actual.provisionApi, aws: vi.fn(), connectAws: vi.fn(), updateCaps: vi.fn(), disconnect: vi.fn() },
  };
});

const policy = { Version: "2012-10-17", Statement: [{ Effect: "Allow", Action: ["ec2:RunInstances"], Resource: "*" }] };
const status = (connected: boolean): AwsStatus => ({
  connected,
  connectedAt: connected ? new Date().toISOString() : null,
  connectedBy: connected ? "Ada" : null,
  caps: { maxServers: 3, maxMonthlyCents: 25_000 },
  usage: { servers: 2, monthlyCents: 4_123 },
  keyUrl: "https://console.aws.amazon.com/iam/home#/users",
  iamPolicy: policy,
});

function show(canEdit = true) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <AwsIntegration workspaceId="ws1" canEdit={canEdit} />
    </QueryClientProvider>,
  );
}
afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("AwsIntegration", () => {
  it("shows the policy and sends the trimmed key to connect", async () => {
    vi.mocked(provisionApi.aws).mockResolvedValue(status(false));
    vi.mocked(provisionApi.connectAws).mockResolvedValue(status(true));
    show();
    expect(await screen.findByText("Not connected")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));

    const dialogPolicy = await screen.findByLabelText("IAM policy");
    expect(dialogPolicy.textContent).toBe(JSON.stringify(policy, null, 2));
    expect(screen.getByRole("button", { name: "Copy policy" })).toBeTruthy();
    expect(screen.getByRole("link", { name: "IAM users" }).getAttribute("href")).toBe(status(false).keyUrl);

    const verify = screen.getByRole("button", { name: "Verify and connect" }) as HTMLButtonElement;
    expect(verify.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Access key ID"), { target: { value: " AKIAEXAMPLE000000000 " } });
    fireEvent.change(screen.getByLabelText("Secret access key"), { target: { value: "secret-value-0123456789" } });
    expect(verify.disabled).toBe(false);
    fireEvent.click(verify);
    await waitFor(() => expect(provisionApi.connectAws).toHaveBeenCalledWith("ws1", {
      accessKeyId: "AKIAEXAMPLE000000000",
      secretAccessKey: "secret-value-0123456789",
    }));
  });

  it("shows limits and spend in exact USD once connected, and saves them through the AWS route", async () => {
    vi.mocked(provisionApi.aws).mockResolvedValue(status(true));
    vi.mocked(provisionApi.updateCaps).mockResolvedValue(status(true));
    show();
    expect(await screen.findByText("Connected")).toBeTruthy();
    expect(screen.getByText(/In use: 2 servers, \$41\.23 per month \(compute, disk and Elastic IPs/)).toBeTruthy();
    expect((screen.getByLabelText("Monthly limit ($)") as HTMLInputElement).value).toBe("250");
    fireEvent.change(screen.getByLabelText("Max servers"), { target: { value: "4" } });
    fireEvent.click(screen.getByRole("button", { name: "Save limits" }));
    await waitFor(() =>
      expect(provisionApi.updateCaps).toHaveBeenCalledWith("ws1", "aws", { maxServers: 4, maxMonthlyCents: 25_000 }),
    );
  });

  it("disconnects through the AWS route, and hides the controls from non-admins", async () => {
    vi.mocked(provisionApi.aws).mockResolvedValue(status(true));
    vi.mocked(provisionApi.disconnect).mockResolvedValue(status(false));
    show();
    fireEvent.click(await screen.findByRole("button", { name: "Disconnect" }));
    await waitFor(() => expect(provisionApi.disconnect).toHaveBeenCalledWith("ws1", "aws"));
    cleanup();

    show(false);
    await screen.findByText("Connected");
    expect(screen.queryByRole("button", { name: "Disconnect" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Replace access key" })).toBeNull();
  });
});
