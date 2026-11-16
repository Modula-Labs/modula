import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  provisionApi,
  type ProviderCatalog,
  type ProvisionedList,
  type ProvisionedServer,
} from "@/lib/provision";
import { AddServerDialog } from "./AddServerDialog";

vi.mock("@/lib/provision", async (orig) => {
  const actual = await orig<typeof import("@/lib/provision")>();
  return {
    ...actual,
    provisionApi: {
      ...actual.provisionApi,
      servers: vi.fn(),
      catalog: vi.fn(),
      sshKeys: vi.fn(),
      provision: vi.fn(),
      server: vi.fn(),
    },
  };
});

const loc = (id: string, city: string, countryCode: string) => ({
  region: id.split(":")[1]!,
  location: { id, label: city, provider: id.split(":")[0]!, countryCode, city },
});
const type = (name: string, location: string, monthlyCents: number, diskGb: number) => ({
  name,
  description: name,
  cores: 2,
  memoryGb: 2,
  diskGb,
  arch: "x86",
  cpuType: "shared",
  prices: [{ location, monthlyCents }],
});

const catalogs: Record<string, ProviderCatalog> = {
  hetzner: {
    locations: [loc("hetzner:fsn1", "Falkenstein", "DE")],
    serverTypes: [type("cx22", "hetzner:fsn1", 452, 40)],
  },
  aws: {
    locations: [loc("aws:af-south-1", "Cape Town", "ZA"), loc("aws:us-east-1", "Ashburn", "US")],
    serverTypes: [type("t3.small", "aws:us-east-1", 2000, 80)],
  },
};
const connected = (kind: string) => ({
  kind,
  caps: { maxServers: 5, maxMonthlyCents: 10_000 },
  usage: { servers: 1, monthlyCents: 452 },
});
const list = (...kinds: string[]): ProvisionedList => ({
  servers: [],
  connectedProviders: kinds.map(connected),
});

const api = vi.mocked(provisionApi);

function open() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <AddServerDialog open onOpenChange={() => {}} workspaceId="ws1" />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  api.catalog.mockImplementation(async (_ws, kind) => catalogs[kind]!);
  api.sshKeys.mockImplementation(async (_ws, kind) =>
    kind === "aws"
      ? [{ id: "eu-west-1/laptop", name: "laptop", fingerprint: "aa:bb" }]
      : [],
  );
  api.provision.mockResolvedValue({ id: "s1", provider: "aws", status: "creating" } as ProvisionedServer);
  api.server.mockResolvedValue({
    id: "s1",
    provider: "aws",
    name: "worker-2",
    serverType: "t3.small",
    location: null,
    monthlyPriceCents: 2000,
    status: "creating",
    ipv4: null,
  } as ProvisionedServer);
});
afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("AddServerDialog providers", () => {
  it("offers every connected provider and starts on the first", async () => {
    api.servers.mockResolvedValue(list("hetzner", "aws"));
    open();
    expect(await screen.findByRole("radio", { name: "Hetzner Cloud" })).toBeTruthy();
    expect(screen.getByRole("radio", { name: "AWS EC2" })).toBeTruthy();
    expect(screen.getByRole("radio", { name: "Manual" })).toBeTruthy();
    expect(screen.getByRole("radio", { name: "Hetzner Cloud" }).getAttribute("aria-checked")).toBe("true");
    expect(await screen.findByText("Falkenstein")).toBeTruthy();
    // Hetzner: prices are converted, the disk comes with the type.
    expect(await screen.findByText("~$4.52")).toBeTruthy();
    expect(screen.getByText(/Hetzner bills in euros/)).toBeTruthy();
    expect(screen.queryByLabelText("Root disk (GB)")).toBeNull();
  });

  it("starts on the only connected provider and leaves out the other", async () => {
    api.servers.mockResolvedValue(list("aws"));
    open();
    expect(await screen.findByRole("radio", { name: "AWS EC2" })).toBeTruthy();
    expect(screen.queryByRole("radio", { name: "Hetzner Cloud" })).toBeNull();
    expect(await screen.findByLabelText("Root disk (GB)")).toBeTruthy();
  });

  it("explains how to connect one when none is, and offers the manual command", async () => {
    api.servers.mockResolvedValue(list());
    open();
    expect(await screen.findByText(/connect\s+Hetzner Cloud or AWS EC2/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Create command" })).toBeTruthy();
    expect(screen.queryByRole("radiogroup", { name: "Provider" })).toBeNull();
  });
});

describe("AddServerDialog on EC2", () => {
  it("picks us-east-1 over the first region listed, shows exact USD and the Elastic IP", async () => {
    api.servers.mockResolvedValue(list("aws"));
    open();
    expect(await screen.findByText("Ashburn")).toBeTruthy();
    expect(screen.getByText("Cape Town")).toBeTruthy();
    expect(screen.getByText("Ashburn").closest("button")?.getAttribute("aria-pressed")).toBe("true");
    expect(await screen.findByText("t3.small")).toBeTruthy();
    expect(screen.getAllByText("$20.00").length).toBeGreaterThan(0);
    expect(screen.queryByText(/~\$/)).toBeNull();
    expect(screen.getByText(/Elastic IP is included/)).toBeTruthy();
    expect(screen.getByText(/the address shows once AWS has attached it/)).toBeTruthy();
  });

  it("moves the price live with the root disk and sends it", async () => {
    api.servers.mockResolvedValue(list("aws"));
    open();
    const disk = (await screen.findByLabelText("Root disk (GB)")) as HTMLInputElement;
    expect(disk.value).toBe("80");
    await screen.findByText("t3.small");
    expect(screen.getByText(/adds \$20\.00/)).toBeTruthy();

    fireEvent.change(disk, { target: { value: "180" } });
    expect(screen.getByText(/adds \$28\.00/)).toBeTruthy();
    expect(screen.getByText("180 GB")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Create server" }));
    await waitFor(() => expect(api.provision).toHaveBeenCalledTimes(1));
    expect(api.provision.mock.calls[0]![1]).toEqual({
      location: "aws:us-east-1",
      serverType: "t3.small",
      name: "worker-1",
      ssh: false,
      sshKeyIds: [],
      diskGb: 180,
    });
  });

  it("refuses a disk outside 20-16384 GB", async () => {
    api.servers.mockResolvedValue(list("aws"));
    open();
    const disk = await screen.findByLabelText("Root disk (GB)");
    await screen.findByText("t3.small");
    fireEvent.change(disk, { target: { value: "10" } });
    expect(screen.getByText("Whole GB from 20 to 16,384")).toBeTruthy();
    expect((screen.getByRole("button", { name: "Create server" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(disk, { target: { value: "20" } });
    expect((screen.getByRole("button", { name: "Create server" }) as HTMLButtonElement).disabled).toBe(false);
    // 20 GB is 60 below the default: 60 * 8 cents off.
    expect(screen.getByText(/adds \$15\.20/)).toBeTruthy();
  });

  it("blocks a server that would pass the monthly limit, with the disk counted", async () => {
    const l = list("aws");
    l.connectedProviders[0]!.caps.maxMonthlyCents = 2500;
    l.connectedProviders[0]!.usage.monthlyCents = 0;
    api.servers.mockResolvedValue(l);
    open();
    const disk = await screen.findByLabelText("Root disk (GB)");
    await screen.findByText("t3.small");
    const create = () => screen.getByRole("button", { name: "Create server" }) as HTMLButtonElement;
    expect(create().disabled).toBe(false);
    fireEvent.change(disk, { target: { value: "200" } });
    expect(screen.getByText(/over the monthly limit/)).toBeTruthy();
    expect(create().disabled).toBe(true);
  });

  it("lists key pairs with their region, and explains when there are none", async () => {
    api.servers.mockResolvedValue(list("aws"));
    open();
    fireEvent.click(await screen.findByLabelText("Open port 22 in the security group"));
    expect(await screen.findByText("laptop")).toBeTruthy();
    expect(screen.getByText("eu-west-1")).toBeTruthy();
    await screen.findByText("t3.small");
    fireEvent.click(screen.getByLabelText(/laptop/));
    fireEvent.click(screen.getByRole("button", { name: "Create server" }));
    await waitFor(() => expect(api.provision).toHaveBeenCalled());
    expect(api.provision.mock.calls[0]![1]).toMatchObject({ ssh: true, sshKeyIds: ["eu-west-1/laptop"] });
  });

  it("shows the provider's own note when it has no SSH keys", async () => {
    api.servers.mockResolvedValue(list("aws"));
    api.sshKeys.mockResolvedValue([]);
    open();
    fireEvent.click(await screen.findByLabelText("Open port 22 in the security group"));
    expect(await screen.findByText(/No key pairs in this AWS account/)).toBeTruthy();
    cleanup();

    api.servers.mockResolvedValue(list("hetzner"));
    open();
    fireEvent.click(await screen.findByLabelText("Open port 22 in the firewall"));
    expect(await screen.findByText(/Hetzner emails a root password/)).toBeTruthy();
  });
});

describe("AddServerDialog on Hetzner", () => {
  it("sends no disk size", async () => {
    api.servers.mockResolvedValue(list("hetzner"));
    open();
    await screen.findByText("cx22");
    fireEvent.click(screen.getByRole("button", { name: "Create server" }));
    await waitFor(() => expect(api.provision).toHaveBeenCalledTimes(1));
    const req = api.provision.mock.calls[0]![1];
    expect(req).toEqual({
      location: "hetzner:fsn1",
      serverType: "cx22",
      name: "worker-1",
      ssh: false,
      sshKeyIds: [],
    });
    expect(req).not.toHaveProperty("diskGb");
  });
});
