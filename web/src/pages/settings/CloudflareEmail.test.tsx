import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "@/lib/api";
import { cloudflare, type EmailSettings } from "@/lib/integrations";
import { CloudflareEmail } from "./CloudflareEmail";

vi.mock("@/lib/integrations", async (orig) => {
  const actual = await orig<typeof import("@/lib/integrations")>();
  return { ...actual, cloudflare: { ...actual.cloudflare, email: vi.fn(), setEmail: vi.fn() } };
});

const ready: EmailSettings = { from: "alerts@example.com", readiness: { state: "ready" } };

function renderEmail(canEdit = true) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <CloudflareEmail workspaceId="ws1" canEdit={canEdit} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("CloudflareEmail", () => {
  it("shows the sender and that it is ready", async () => {
    vi.mocked(cloudflare.email).mockResolvedValue(ready);
    renderEmail();
    expect(((await screen.findByLabelText("Sender address")) as HTMLInputElement).value).toBe("alerts@example.com");
    expect(screen.getByText("Ready to send")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Check again" })).toBeNull();
  });

  it("saves a new sender and shows what Cloudflare still needs", async () => {
    vi.mocked(cloudflare.email).mockResolvedValue({ from: null, readiness: null });
    vi.mocked(cloudflare.setEmail).mockResolvedValue({ from: "alerts@example.com", readiness: { state: "not_onboarded" } });
    renderEmail();
    const input = (await screen.findByLabelText("Sender address")) as HTMLInputElement;
    const save = screen.getByRole("button", { name: "Save" }) as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    fireEvent.change(input, { target: { value: " alerts@example.com " } });
    fireEvent.click(save);
    await waitFor(() => expect(cloudflare.setEmail).toHaveBeenCalledWith("ws1", "alerts@example.com"));
    expect(await screen.findByText("Onboard example.com under Email Service → Email Sending in Cloudflare")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Check again" })).toBeTruthy();
  });

  it("clears the sender with an empty field, and shows the server's refusal", async () => {
    vi.mocked(cloudflare.email).mockResolvedValue(ready);
    vi.mocked(cloudflare.setEmail).mockRejectedValueOnce(new ApiError(400, "bad_request", "the sender must be an email address"));
    renderEmail();
    const input = await screen.findByLabelText("Sender address");
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(cloudflare.setEmail).toHaveBeenCalledWith("ws1", null));
    expect(await screen.findByText("the sender must be an email address")).toBeTruthy();
  });

  it("is read-only for members", async () => {
    vi.mocked(cloudflare.email).mockResolvedValue({ from: "alerts@example.com", readiness: { state: "no_zone" } });
    renderEmail(false);
    const input = (await screen.findByLabelText("Sender address")) as HTMLInputElement;
    expect(input.disabled).toBe(true);
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    expect(screen.getByText("example.com isn't a zone this token can reach")).toBeTruthy();
  });
});
