import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Checkbox, Radio } from "@/components/ui/Controls";
import { Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Skeleton } from "@/components/ui/misc";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/Table";
import { Segmented } from "@/components/ui/Tabs";
import { flagEmoji } from "@/lib/flags";
import { formatBytes } from "@/lib/format";
import { migrateApi, type MigrationPreview, type MigrationTarget, type PreviewParams } from "@/lib/migrate";
import { approxUsd, providerLabel, provisionApi, typesIn } from "@/lib/provision";
import { useCurrentWorkspace, useMe } from "@/lib/workspace";
import { MigrationProgress } from "./MigrationProgress";

/**
 * Migrate a server to a new one at any connected provider, or to one that
 * exists: pick where and what, read what moves and what changes, confirm.
 * Then shows the migration's progress. The main server can be moved too: its dashboard goes read-only while the control plane is copied.
 */
export function MigrateDialog({
  open,
  onOpenChange,
  nodeId,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  nodeId: string;
}) {
  const first = useQuery({
    queryKey: ["migration-preview", nodeId],
    queryFn: () => migrateApi.preview(nodeId),
    enabled: open,
  });
  // A shared server added by hand migrates on behalf of a workspace you pick.
  const me = useMe();
  const [current] = useCurrentWorkspace(me.data);
  const [picked, setPicked] = useState<string | null>(null);
  const named = first.data?.workspaceRequired ? (picked ?? current?.id ?? null) : null;
  const scoped = useQuery({
    queryKey: ["migration-preview", nodeId, named],
    queryFn: () => migrateApi.preview(nodeId, { workspaceId: named! }),
    enabled: open && !!named,
  });
  const preview = named ? scoped : first;
  const [started, setStarted] = useState<string | null>(null);
  const running = started ?? preview.data?.activeMigrationId ?? null;
  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) setStarted(null);
  };

  return (
    <Modal
      open={open}
      onOpenChange={close}
      width={running ? 640 : 720}
      title={running ? "Migrating server" : `Migrate ${preview.data?.name ?? "server"}`}
      description={
        running
          ? "Services move one at a time; ones with volumes are down while their data is copied."
          : "Move everything on this server to a new one or one you already have, volumes' data included. The old server is kept until you delete it."
      }
    >
      {running ? (
        <MigrationProgress migrationId={running} onDone={() => close(false)} />
      ) : preview.isPending ? (
        <Skeleton className="h-48" />
      ) : preview.error ? (
        <Alert>{preview.error.message}</Alert>
      ) : preview.data.main && preview.data.mainBlocker ? (
        <Alert tone="info" title="The main server can't be migrated right now">
          {preview.data.mainBlocker}
        </Alert>
      ) : !preview.data.workspaceId ? (
        <Alert tone="info" title="Choose a workspace">
          This shared server was added by hand; join a workspace to migrate it on its behalf.
        </Alert>
      ) : (
        <div className="flex flex-col gap-4">
        {named && (me.data?.workspaces.length ?? 0) > 1 ? (
          <label className="flex items-center justify-between gap-3 text-sm">
            <span className="font-bold">Migrate for workspace</span>
            <Select className="w-56" value={named} onChange={(e) => setPicked(e.target.value)}>
              {me.data!.workspaces.map((w) => (
                <option key={w.id} value={w.id}>
                  {w.name}
                </option>
              ))}
            </Select>
          </label>
        ) : null}
        <MigrateForm
          key={preview.data.workspaceId}
          preview={preview.data}
          workspaceId={preview.data.workspaceId}
          named={named ?? undefined}
          onStarted={setStarted}
          onCancel={() => close(false)}
        />
        </div>
      )}
    </Modal>
  );
}

function MigrateForm({
  preview,
  workspaceId,
  named,
  onStarted,
  onCancel,
}: {
  preview: MigrationPreview;
  workspaceId: string;
  /** Set when the request has to name the workspace. */
  named?: string;
  onStarted: (id: string) => void;
  onCancel: () => void;
}) {
  const qc = useQueryClient();
  // The main server needs a fresh machine.
  const canUseExisting = !preview.main && preview.existingTargets.length > 0;
  const canCreate = preview.connectedProviders.length > 0;
  const [mode, setMode] = useState<"new" | "existing">(canCreate || !canUseExisting ? "new" : "existing");
  const [provider, setProvider] = useState(preview.connectedProviders[0] ?? "");
  const [nodeTarget, setNodeTarget] = useState<string | null>(null);
  const [diskGb, setDiskGb] = useState("");
  const catalog = useQuery({
    queryKey: ["provider-catalog", workspaceId, provider],
    queryFn: () => provisionApi.catalog(workspaceId, provider),
    enabled: canCreate,
    staleTime: 5 * 60_000,
  });
  const [location, setLocation] = useState<string | null>(null);
  const [serverType, setServerType] = useState<string | null>(null);
  const [understood, setUnderstood] = useState(false);
  const [understoodMain, setUnderstoodMain] = useState(false);
  // Records left on the old address; everything is repointed by default.
  const [dnsSkip, setDnsSkip] = useState<Set<string>>(new Set());
  const toggleDns = (id: string, on: boolean) =>
    setDnsSkip((prev) => {
      const next = new Set(prev);
      if (on) next.delete(id);
      else next.add(id);
      return next;
    });

  // Default: the first location that isn't the current one.
  useEffect(() => {
    if (location || !catalog.data) return;
    const other = catalog.data.locations.find((l) => l.location.id !== preview.location?.id);
    setLocation((other ?? catalog.data.locations[0])?.location.id ?? null);
  }, [catalog.data, location, preview.location]);

  const types = useMemo(
    () => (location ? typesIn(catalog.data?.serverTypes ?? [], location) : []),
    [catalog.data, location],
  );
  useEffect(() => {
    if (types.length && !types.some((t) => t.name === serverType)) setServerType(types[0]!.name);
  }, [types, serverType]);
  const chosen = types.find((t) => t.name === serverType);

  const disk = provider === "aws" && diskGb ? Number(diskGb) : undefined;
  const target: MigrationTarget | null =
    mode === "existing"
      ? nodeTarget
        ? { kind: "existing", nodeId: nodeTarget }
        : null
      : location && serverType
        ? { kind: "new", provider, location, serverType, ...(disk ? { diskGb: disk } : {}) }
        : null;
  const checks: PreviewParams | null = !target
    ? null
    : target.kind === "existing"
      ? { workspaceId: named, targetNodeId: target.nodeId }
      : { workspaceId: named, location: target.location, serverType: target.serverType, diskGb: target.diskGb };
  // What this target changes: architecture, disk, room.
  const checked = useQuery({
    queryKey: ["migration-preview", preview.nodeId, "target", checks],
    queryFn: () => migrateApi.preview(preview.nodeId, checks!),
    enabled: !!checks,
    staleTime: 30_000,
  });
  const warnings = (checked.data ?? preview).warnings.filter((w) => w.code !== "tcp_address");
  const blocked = warnings.some((w) => w.blocking);

  const migrate = useMutation({
    mutationFn: () =>
      migrateApi.migrate(preview.nodeId, {
        ...(named ? { workspaceId: named } : {}),
        target: target!,
        dnsSkip: [...dnsSkip],
      }),
    onSuccess: (m) => {
      qc.invalidateQueries({ queryKey: ["provisioned", workspaceId] });
      qc.invalidateQueries({ queryKey: ["migration-preview", preview.nodeId] });
      onStarted(m.id);
    },
  });

  const stateful = preview.services.filter((s) => !s.replicaOnly && s.volumes.length);
  const stateless = preview.services.filter((s) => s.replicaOnly || !s.volumes.length);
  const proxies = preview.services.filter((s) => s.tcpAddress);
  const unmeasured = stateful.some((s) => s.volumes.some((v) => v.usedBytes === null));

  if (canCreate && mode === "new" && catalog.isPending) return <Skeleton className="h-64" />;
  if (canCreate && mode === "new" && catalog.error) return <Alert>{catalog.error.message}</Alert>;

  return (
    <div className="flex flex-col gap-5">
      {migrate.error ? <Alert>{migrate.error.message}</Alert> : null}

      {canUseExisting && canCreate ? (
        <Segmented
          label="Target"
          value={mode}
          onChange={setMode}
          options={[
            { value: "new", label: "New server" },
            { value: "existing", label: "Existing server" },
          ]}
        />
      ) : null}

      {!canCreate && !canUseExisting ? (
        <Alert tone="info" title="Nowhere to move to">
          Connect a cloud provider in workspace settings, or add another server.
        </Alert>
      ) : null}

      {mode === "existing" ? (
        <fieldset className="flex flex-col gap-2">
          <legend className="mb-1.5 text-sm font-bold">Move to</legend>
          <div className="flex flex-col gap-1.5">
            {preview.existingTargets.map((t) => (
              <label
                key={t.nodeId}
                className={clsx(
                  "flex items-center gap-2.5 rounded-lg border px-3 py-2.5 text-sm",
                  nodeTarget === t.nodeId ? "border-foreground bg-muted" : "border-border",
                  t.blocker ? "opacity-60" : "cursor-pointer hover:bg-muted/50",
                )}
              >
                <Radio
                  name="migrate-existing"
                  value={t.nodeId}
                  disabled={!!t.blocker}
                  checked={nodeTarget === t.nodeId}
                  onChange={() => setNodeTarget(t.nodeId)}
                />
                <span className="min-w-0">
                  <span className="block font-mono">{t.name}</span>
                  <span className="block text-xs text-muted-foreground">
                    {t.blocker ??
                      `${t.location?.city ?? "Unknown location"} · ${t.freeCpuMillis / 1000} vCPU, ${Math.round(t.freeMemoryMb / 1024)} GB free${t.arch ? ` · ${t.arch}` : ""}`}
                  </span>
                </span>
              </label>
            ))}
          </div>
        </fieldset>
      ) : canCreate ? (
        <>
      {preview.connectedProviders.length > 1 ? (
        <Segmented
          label="Provider"
          value={provider}
          onChange={(p) => {
            setProvider(p);
            setLocation(null);
            setServerType(null);
          }}
          options={preview.connectedProviders.map((k) => ({ value: k, label: providerLabel(k) }))}
        />
      ) : null}
      <fieldset className="flex flex-col gap-2">
        <legend className="mb-1.5 text-sm font-bold">New location</legend>
        <div className="grid grid-cols-3 gap-2">
          {(catalog.data?.locations ?? []).map((l) => (
            <button
              key={l.location.id}
              type="button"
              aria-pressed={location === l.location.id}
              onClick={() => setLocation(l.location.id)}
              className={clsx(
                "flex items-center gap-2.5 rounded-lg border px-3 py-2.5 text-left text-sm",
                location === l.location.id ? "border-foreground bg-muted" : "border-border hover:bg-muted/50",
              )}
            >
              <span aria-hidden className="text-base">
                {flagEmoji(l.location.countryCode)}
              </span>
              <span className="min-w-0">
                <span className="block truncate">
                  {l.location.city}
                  {l.location.id === preview.location?.id ? (
                    <span className="text-muted-foreground"> · now</span>
                  ) : null}
                </span>
                <span className="block font-mono text-xs text-muted-foreground">{l.region}</span>
              </span>
            </button>
          ))}
        </div>
      </fieldset>

      <fieldset className="flex flex-col gap-2">
        <legend className="mb-1.5 text-sm font-bold">Server type</legend>
        {types.length === 0 ? (
          <p className="text-sm text-muted-foreground">No server types are available in this location.</p>
        ) : (
          <div className="max-h-48 overflow-auto rounded-xl">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Type</TableHead>
                  <TableHead className="text-right">vCPU</TableHead>
                  <TableHead className="text-right">RAM</TableHead>
                  <TableHead className="text-right">Disk</TableHead>
                  <TableHead className="text-right">Per month</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {types.map((t) => (
                  <TableRow
                    key={t.name}
                    onClick={() => setServerType(t.name)}
                    data-selected={serverType === t.name}
                    className="cursor-pointer"
                  >
                    <TableCell>
                      <label className="flex items-center gap-2">
                        <Radio
                          name="migrate-server-type"
                          value={t.name}
                          checked={serverType === t.name}
                          onChange={() => setServerType(t.name)}
                        />
                        <span className="font-mono">{t.name}</span>
                        <span className="text-xs text-muted-foreground">{t.arch === "arm" ? "Arm" : "x86"}</span>
                      </label>
                    </TableCell>
                    <TableCell className="text-right tabular-nums">{t.cores}</TableCell>
                    <TableCell className="text-right tabular-nums">{t.memoryGb} GB</TableCell>
                    <TableCell className="text-right tabular-nums">{t.diskGb} GB</TableCell>
                    <TableCell className="text-right tabular-nums">{approxUsd(t.monthlyCents)}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
      </fieldset>

      {provider === "aws" ? (
        <label className="flex items-center justify-between gap-3 text-sm">
          <span className="font-bold">Root disk (GB)</span>
          <Input
            className="w-28"
            type="number"
            min={8}
            placeholder={String(chosen?.diskGb ?? 80)}
            value={diskGb}
            onChange={(e) => setDiskGb(e.target.value)}
          />
        </label>
      ) : null}
        </>
      ) : null}

      <section className="flex flex-col gap-2 text-sm">
        <h3 className="font-bold">What happens</h3>
        {warnings.map((w) => (
          <Alert key={`${w.code}:${w.message}`} tone={w.blocking ? undefined : "warn"}>
            {w.message}
          </Alert>
        ))}
        {preview.services.length === 0 ? (
          <p className="text-muted-foreground">Nothing runs on this server; only the new server is created.</p>
        ) : null}
        {stateful.length ? (
          <div>
            <p>
              <span className="font-medium">Down while their data copies</span>{" "}
              <span className="text-muted-foreground">
                ({formatBytes(preview.totalVolumeBytes)}
                {unmeasured ? ", some not measured yet" : ""}):
              </span>
            </p>
            <ul className="mt-1 flex flex-col gap-0.5 text-[13px] text-muted-foreground">
              {stateful.map((s) => (
                <li key={s.instanceId}>
                  {s.service} <span className="opacity-70">({s.project} · {s.environment})</span> —{" "}
                  {s.volumes.map((v) => `${v.mountPath} ${formatBytes(v.usedBytes)}`).join(", ")}
                </li>
              ))}
            </ul>
          </div>
        ) : null}
        {stateless.length ? (
          <p>
            <span className="font-medium">Redeployed on the new server</span>{" "}
            <span className="text-muted-foreground">
              (they keep serving until healthy there): {stateless.map((s) => s.service).join(", ")}
            </span>
          </p>
        ) : null}
        {preview.main ? (
          <Alert tone="warn" title="The dashboard moves too">
            <p>
              This server runs the dashboard. It is unavailable for a few minutes while the
              control plane is copied and started on the new server: changes are refused
              meanwhile. Apps keep running, except while their own data moves.
            </p>
            {preview.ipBased ? (
              <p className="mt-1.5">
                The dashboard is reached by IP, so its address changes from{" "}
                <span className="font-mono">{preview.currentUrl}</span> to the new server's IP.
                Run <span className="font-mono">modula login &lt;new address&gt;</span> afterwards
                and update your bookmarks.
              </p>
            ) : null}
            {preview.dashboardHosts.length ? (
              <div className="mt-1.5">
                <p>
                  These hostnames must point at the new server. They are repointed automatically
                  when managed in a connected Cloudflare zone; otherwise set their A records to the
                  new server's IP yourself:
                </p>
                <ul className="mt-1 font-mono text-[13px]">
                  {preview.dashboardHosts.map((h) => (
                    <li key={h}>{h}</li>
                  ))}
                </ul>
              </div>
            ) : null}
            <p className="mt-1.5">
              DNS caches may need a few minutes; if the old address sticks, flush your local DNS
              cache.
            </p>
          </Alert>
        ) : null}
        {proxies.length ? (
          <Alert tone="warn" title="TCP proxy addresses change">
            They move to the new server's IP address; clients using these must be updated:
            <ul className="mt-1 font-mono text-[13px]">
              {proxies.map((s) => (
                <li key={s.instanceId}>
                  {s.service}: {s.tcpAddress}
                </li>
              ))}
            </ul>
          </Alert>
        ) : null}
        {preview.dns.records.length ? (
          <div className="flex flex-col gap-1.5">
            <p>
              <span className="font-medium">DNS records repointed to the new server</span>{" "}
              <span className="text-muted-foreground">(untick to leave one on the old address)</span>
            </p>
            <div className="max-h-48 overflow-auto rounded-xl">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Name</TableHead>
                    <TableHead>Zone</TableHead>
                    <TableHead>Now</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {preview.dns.records.map((r) => (
                    <TableRow key={r.id}>
                      <TableCell>
                        <Checkbox
                          checked={!dnsSkip.has(r.id)}
                          onCheckedChange={(on) => toggleDns(r.id, on)}
                          label={r.name}
                        />
                      </TableCell>
                      <TableCell className="text-muted-foreground">{r.zone}</TableCell>
                      <TableCell className="font-mono text-[13px]">
                        {r.content}
                        {r.proxied ? <span className="text-muted-foreground"> · proxied</span> : null}
                        {r.managed ? null : <span className="text-muted-foreground"> · yours</span>}
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
              </Table>
            </div>
          </div>
        ) : null}
        {preview.dns.aaaaNames.length ? (
          <Alert tone="warn" title="IPv6 records stay as they are">
            The new server has no IPv6 address known to Modula, so these names' AAAA records keep
            pointing at the old server. Remove or update them:{" "}
            <span className="font-mono text-[13px]">{preview.dns.aaaaNames.join(", ")}</span>
          </Alert>
        ) : null}
        {preview.dns.error ? (
          <Alert tone="warn" title="Couldn't read Cloudflare">
            {preview.dns.error}. The migration can still start; DNS is checked again when the
            services have moved.
          </Alert>
        ) : null}
        <p className="text-[13px] text-muted-foreground">
          {preview.dns.connected
            ? "Custom domains and the records above follow at the end of the migration. Names outside your Cloudflare zones must be updated by hand."
            : "No DNS provider is connected: update your records to the new server's address yourself."}{" "}
          {preview.name} keeps forwarding moved domains and stays billed until you delete it after
          the migration.
        </p>
      </section>

      {preview.main ? (
        <Checkbox
          checked={understoodMain}
          onCheckedChange={setUnderstoodMain}
          label="I understand the dashboard is unavailable for a few minutes and its address may change"
        />
      ) : null}
      <Checkbox
        checked={understood}
        onCheckedChange={setUnderstood}
        label="I understand services with volumes are unavailable during their copy"
      />

      <div className="flex items-center justify-between gap-4 border-t border-border pt-4">
        <p className="text-[13px] text-muted-foreground">
          {mode === "new" && chosen ? `New server: ${approxUsd(chosen.monthlyCents)} per month` : ""}
        </p>
        <div className="flex gap-2">
          <Button variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button
            variant="primary"
            loading={migrate.isPending}
            disabled={!target || blocked || (stateful.length > 0 && !understood) || (preview.main && !understoodMain)}
            onClick={() => migrate.mutate()}
          >
            Migrate server
          </Button>
        </div>
      </div>
    </div>
  );
}
