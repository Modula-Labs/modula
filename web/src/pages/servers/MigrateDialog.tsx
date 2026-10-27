import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Checkbox, Radio } from "@/components/ui/Controls";
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
import { flagEmoji } from "@/lib/flags";
import { formatBytes } from "@/lib/format";
import { migrateApi, type MigrationPreview } from "@/lib/migrate";
import { euros, provisionApi, typesIn } from "@/lib/provision";
import { MigrationProgress } from "./MigrationProgress";

/**
 * Migrate a worker server to a new Hetzner server (another location or
 * size): pick where and what, read what moves and what changes, confirm.
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
  const preview = useQuery({
    queryKey: ["migration-preview", nodeId],
    queryFn: () => migrateApi.preview(nodeId),
    enabled: open,
  });
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
          : "Create a new server at Hetzner and move everything on this one to it, volumes' data included. The old server is kept until you delete it."
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
      ) : !preview.data.serverId || !preview.data.workspaceId ? (
        <Alert tone="info" title="Only servers linked to Hetzner can be migrated">
          This server was added by hand. Move its services one at a time
          instead, or add a server and move them there.
        </Alert>
      ) : (
        <MigrateForm
          preview={preview.data}
          workspaceId={preview.data.workspaceId}
          onStarted={setStarted}
          onCancel={() => close(false)}
        />
      )}
    </Modal>
  );
}

function MigrateForm({
  preview,
  workspaceId,
  onStarted,
  onCancel,
}: {
  preview: MigrationPreview;
  workspaceId: string;
  onStarted: (id: string) => void;
  onCancel: () => void;
}) {
  const qc = useQueryClient();
  const catalog = useQuery({
    queryKey: ["hetzner-catalog", workspaceId],
    queryFn: () => provisionApi.hetznerCatalog(workspaceId),
    staleTime: 5 * 60_000,
  });
  const [location, setLocation] = useState<string | null>(null);
  const [serverType, setServerType] = useState<string | null>(null);
  const [understood, setUnderstood] = useState(false);
  const [understoodMain, setUnderstoodMain] = useState(false);

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

  const migrate = useMutation({
    mutationFn: () => migrateApi.migrate(preview.nodeId, { location: location!, serverType: serverType! }),
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

  if (catalog.isPending) return <Skeleton className="h-64" />;
  if (catalog.error) return <Alert>{catalog.error.message}</Alert>;

  return (
    <div className="flex flex-col gap-5">
      {migrate.error ? <Alert>{migrate.error.message}</Alert> : null}

      <fieldset className="flex flex-col gap-2">
        <legend className="mb-1.5 text-sm font-bold">New location</legend>
        <div className="grid grid-cols-3 gap-2">
          {catalog.data.locations.map((l) => (
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
                    <TableCell className="text-right tabular-nums">{euros(t.monthlyCents)}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
        {chosen && preview.totalVolumeBytes > chosen.diskGb * 1e9 * 0.8 ? (
          <p className="text-[13px] text-error">
            The volumes ({formatBytes(preview.totalVolumeBytes)}) would nearly fill this server's disk.
          </p>
        ) : null}
      </fieldset>

      <section className="flex flex-col gap-2 text-sm">
        <h3 className="font-bold">What happens</h3>
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
        <p className="text-[13px] text-muted-foreground">
          Custom domains' DNS records follow automatically when the workspace's DNS is on
          Cloudflare; otherwise update A records to the new server. {preview.name} keeps running
          (and is billed) until you delete it after the migration.
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
          {chosen ? `New server: ${euros(chosen.monthlyCents)} per month` : ""}
        </p>
        <div className="flex gap-2">
          <Button variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button
            variant="primary"
            loading={migrate.isPending}
            disabled={!chosen || !location || (stateful.length > 0 && !understood) || (preview.main && !understoodMain)}
            onClick={() => migrate.mutate()}
          >
            Migrate server
          </Button>
        </div>
      </div>
    </div>
  );
}
