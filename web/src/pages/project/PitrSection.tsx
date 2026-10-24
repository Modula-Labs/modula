import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Checkbox, Radio, RadioGroup } from "@/components/ui/Controls";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton, type Tone } from "@/components/ui/misc";
import { api, type BackupDestinations, type Instance, type PitrInfo, type PitrRestore, type PitrRestored } from "@/lib/api";
import { defaultDestination, destinationChoice, destinationValue, hourLabel, type DestinationGroup } from "@/lib/backup";
import { formatBytes, relativeTime } from "@/lib/format";
import { fromLocalInput, inWindow, windowBounds } from "@/lib/pitr";
import { DestinationField } from "./DestinationField";

const restoreTone: Record<PitrRestore["status"], Tone> = { running: "info", completed: "success", failed: "danger" };
const restoreLabel: Record<PitrRestore["status"], string> = { running: "Running", completed: "Completed", failed: "Failed" };

interface PitrForm {
  enabled: boolean;
  destination: string;
  windowDays: number;
  hourUtc: number;
}

const stamp = (iso: string) => new Date(iso).toLocaleString();

/** Point-in-time recovery for a Postgres service: WAL archiving, base backups and restores. */
export function PitrSection({
  instance,
  canEdit,
  dests,
  groups,
}: {
  instance: Instance;
  canEdit: boolean;
  dests: BackupDestinations;
  groups: DestinationGroup[];
}) {
  const qc = useQueryClient();
  const key = ["pitr", instance.id];
  const info = useQuery({
    queryKey: key,
    queryFn: () => api.pitr(instance.id),
    refetchInterval: (q) => {
      const d = q.state.data;
      const busy = d?.restores.some((r) => r.status === "running") || d?.baseBackups.some((b) => b.status === "running" || b.status === "deleting");
      return busy ? 3000 : false;
    },
  });
  const config = info.data?.config ?? null;
  const [form, setForm] = useState<PitrForm>({ enabled: false, destination: "", windowDays: 7, hourUtc: 3 });
  useEffect(() => {
    setForm({
      enabled: config?.enabled ?? false,
      destination: config ? destinationValue(config.targetId, dests) : "",
      windowDays: config?.windowDays ?? 7,
      hourUtc: config?.hourUtc ?? 3,
    });
  }, [config, dests]);
  const destination = form.destination || defaultDestination(groups);
  const choice = destinationChoice(destination);

  const save = useMutation({
    mutationFn: () =>
      api.savePitr(instance.id, { enabled: form.enabled, windowDays: form.windowDays, hourUtc: form.hourUtc, ...(choice ?? {}) }),
    onSuccess: (data) => {
      qc.setQueryData(key, data);
      void qc.invalidateQueries({ queryKey: ["backup-destinations", instance.id] });
      void qc.invalidateQueries({ queryKey: ["deployments", instance.id] });
    },
  });
  const baseBackup = useMutation({ mutationFn: () => api.pitrBaseBackup(instance.id), onSuccess: (data) => qc.setQueryData(key, data) });
  const [restoring, setRestoring] = useState(false);

  const dirty =
    !config ||
    config.enabled !== form.enabled ||
    destinationValue(config.targetId, dests) !== destination ||
    config.windowDays !== form.windowDays ||
    config.hourUtc !== form.hourUtc;
  const windowValid = Number.isInteger(form.windowDays) && form.windowDays >= 1 && form.windowDays <= 35;
  const canSave = dirty && windowValid && (!!config || (form.enabled && !!choice));

  if (info.isPending) return <Skeleton className="h-32" />;
  if (info.error) return <Alert tone="danger">{info.error.message}</Alert>;
  const data = info.data;
  const error = save.error ?? baseBackup.error;

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h3 className="text-base leading-6 font-bold">Point-in-time recovery</h3>
          <p className="mt-1 text-sm text-muted-foreground">
            Archives the write-ahead log so the database can be restored to any moment inside the window. Turning it on or off redeploys the database.
          </p>
        </div>
        {canEdit && config?.enabled ? (
          <div className="flex shrink-0 gap-2">
            <Button variant="secondary" loading={baseBackup.isPending} onClick={() => baseBackup.mutate()}>
              Take base backup
            </Button>
            <Button variant="secondary" disabled={!data.window} onClick={() => setRestoring(true)}>
              Restore to point in time
            </Button>
          </div>
        ) : null}
      </div>
      {error ? <Alert>{error.message}</Alert> : null}

      {config?.enabled ? <WindowSummary data={data} /> : null}

      <Checkbox
        disabled={!canEdit}
        checked={form.enabled}
        onCheckedChange={(enabled) => setForm((f) => ({ ...f, enabled }))}
        label="Archive WAL for point-in-time recovery"
      />
      {groups.length > 0 ? (
        <div className="grid grid-cols-3 gap-3">
          <DestinationField
            groups={groups}
            dests={dests}
            value={destination}
            disabled={!canEdit}
            onChange={(v) => setForm((f) => ({ ...f, destination: v }))}
          />
          <Field label="Keep (days)" hint="1 to 35.">
            {(p) => (
              <Input
                {...p}
                type="number"
                min={1}
                max={35}
                disabled={!canEdit}
                value={form.windowDays}
                onChange={(e) => setForm((f) => ({ ...f, windowDays: Math.floor(Number(e.target.value)) || 0 }))}
              />
            )}
          </Field>
          <Field label="Base backup time">
            {(p) => (
              <Select {...p} disabled={!canEdit} value={form.hourUtc} onChange={(e) => setForm((f) => ({ ...f, hourUtc: Number(e.target.value) }))}>
                {Array.from({ length: 24 }, (_, h) => (
                  <option key={h} value={h}>
                    {hourLabel(h)}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        </div>
      ) : (
        <p className="text-sm text-muted-foreground">Add backup storage, a bucket service or use this server's disk to turn this on.</p>
      )}
      {canEdit ? (
        <div>
          <Button variant="secondary" loading={save.isPending} disabled={!canSave} onClick={() => save.mutate()}>
            Save
          </Button>
        </div>
      ) : null}

      {data.baseBackups.length > 0 ? (
        <div className="flex flex-col gap-2">
          <h4 className="text-sm font-bold">Base backups</h4>
          <div className="divide-y divide-border overflow-hidden rounded-xl border border-border">
            {data.baseBackups.map((b) => (
              <div key={b.id} className="flex min-h-12 items-center gap-3 px-3 py-2">
                <Badge tone={b.status === "completed" ? "success" : b.status === "failed" ? "danger" : b.status === "running" ? "info" : "neutral"}>
                  {b.status === "completed" ? "Completed" : b.status === "failed" ? "Failed" : b.status === "running" ? "Running" : b.status === "deleting" ? "Deleting" : "Deleted"}
                </Badge>
                <div className="min-w-0 flex-1">
                  <div className="text-sm">
                    <span title={stamp(b.startedAt)}>{relativeTime(b.startedAt)}</span>
                    <span className="text-[13px] text-muted-foreground"> · {b.kind === "manual" ? "Manual" : "Scheduled"} · to {b.targetName}</span>
                  </div>
                  {b.error ? <div className="mt-0.5 text-[13px] break-words text-error">{b.error}</div> : null}
                </div>
                <span className="text-sm text-muted-foreground tabular-nums">{formatBytes(b.sizeBytes)}</span>
              </div>
            ))}
          </div>
        </div>
      ) : null}

      {data.restores.length > 0 ? (
        <div className="flex flex-col gap-2">
          <h4 className="text-sm font-bold">Restores</h4>
          <div className="divide-y divide-border overflow-hidden rounded-xl border border-border">
            {data.restores.map((r) => (
              <div key={r.id} className="flex min-h-12 items-center gap-3 px-3 py-2">
                <Badge tone={restoreTone[r.status]}>{restoreLabel[r.status]}</Badge>
                <div className="min-w-0 flex-1">
                  <div className="text-sm">
                    {r.instanceId === instance.id ? (
                      "This service"
                    ) : (
                      <Link
                        to="/projects/$projectId"
                        params={{ projectId: instance.projectId }}
                        search={{ env: instance.environmentId }}
                        className="underline"
                      >
                        {r.serviceName}
                      </Link>
                    )}
                    <span className="text-[13px] text-muted-foreground"> · to {stamp(r.targetTime)}</span>
                  </div>
                  <div className="text-[13px] text-muted-foreground">
                    <span title={stamp(r.startedAt)}>started {relativeTime(r.startedAt)}</span>
                  </div>
                  {r.error ? <div className="mt-0.5 text-[13px] break-words text-error">{r.error}</div> : null}
                </div>
              </div>
            ))}
          </div>
        </div>
      ) : null}

      {data.window ? (
        <PitrRestoreDialog instance={instance} window={data.window} open={restoring} onClose={() => setRestoring(false)} />
      ) : null}
    </section>
  );
}

function WindowSummary({ data }: { data: PitrInfo }) {
  const a = data.archiving;
  return (
    <div className="flex flex-col gap-1 rounded-xl border border-border px-4 py-3">
      {data.window ? (
        <div className="text-base font-bold">
          Recoverable from {stamp(data.window.from)} to {stamp(data.window.to)}
        </div>
      ) : (
        <div className="text-sm">No recoverable window yet. Take a base backup to start one.</div>
      )}
      {a ? (
        <div className="text-[13px] text-muted-foreground">
          {a.lastArchivedAt ? <span title={stamp(a.lastArchivedAt)}>Last archived {relativeTime(a.lastArchivedAt)}</span> : "Nothing archived yet"}
          {a.failedCount > 0 ? (
            <span className="text-error">
              {" "}
              · {a.failedCount} failed{a.lastFailedAt ? `, last ${relativeTime(a.lastFailedAt)}` : ""}
            </span>
          ) : null}
          {a.inRecovery ? " · In recovery" : null}
        </div>
      ) : (
        <div className="text-[13px] text-muted-foreground">Archiving status is unavailable while the database is not running.</div>
      )}
    </div>
  );
}

function PitrRestoreDialog({
  instance,
  window,
  open,
  onClose,
}: {
  instance: Instance;
  window: { from: string; to: string };
  open: boolean;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const bounds = windowBounds(window);
  const [at, setAt] = useState(bounds.max);
  const [into, setInto] = useState<"new" | "same">("new");
  const [name, setName] = useState("");
  const [typed, setTyped] = useState("");
  const [done, setDone] = useState<PitrRestored | null>(null);

  const reset = () => {
    setAt(windowBounds(window).max);
    setInto("new");
    setName("");
    setTyped("");
    setDone(null);
    restore.reset();
  };
  const restore = useMutation({
    mutationFn: () =>
      api.pitrRestore(instance.id, {
        targetTime: fromLocalInput(at)!,
        into,
        name: into === "new" && name.trim() ? name.trim() : undefined,
        confirm: into === "same" ? typed : undefined,
      }),
    onSuccess: (r) => {
      void qc.invalidateQueries({ queryKey: ["pitr", instance.id] });
      void qc.invalidateQueries({ queryKey: ["deployments", instance.id] });
      void qc.invalidateQueries({ queryKey: ["project", instance.projectId] });
      setDone(r);
    },
  });
  const valid = inWindow(at, window) && (into === "new" || typed === instance.serviceName);
  const close = () => {
    onClose();
    reset();
  };

  return (
    <Modal
      open={open}
      onOpenChange={(o) => {
        if (!o) close();
      }}
      title="Restore to point in time"
      description={`Recoverable from ${stamp(window.from)} to ${stamp(window.to)}.`}
    >
      {done ? (
        <div className="flex flex-col gap-4">
          <Alert tone="success">
            {into === "new" ? (
              <>
                Restoring into{" "}
                <Link
                  to="/projects/$projectId"
                  params={{ projectId: instance.projectId }}
                  search={{ env: instance.environmentId, service: done.serviceId }}
                  className="underline"
                  onClick={close}
                >
                  {done.serviceName}
                </Link>
                . It comes online when the restore finishes.
              </>
            ) : (
              <>Restoring this service. It restarts when the restore finishes.</>
            )}
          </Alert>
          <div className="flex justify-end">
            <Button variant="secondary" onClick={close}>
              Close
            </Button>
          </div>
        </div>
      ) : (
        <form
          className="flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (valid) restore.mutate();
          }}
        >
          {restore.error ? <Alert>{restore.error.message}</Alert> : null}
          <Field
            label="Restore to"
            hint="Your local time."
            error={at && !inWindow(at, window) ? "Pick a time inside the recoverable window" : null}
          >
            {(p) => <Input {...p} type="datetime-local" min={bounds.min} max={bounds.max} value={at} onChange={(e) => setAt(e.target.value)} />}
          </Field>
          <RadioGroup value={into} onValueChange={(v) => setInto(v as "new" | "same")} aria-label="Restore into">
            <Radio value="new" label="Into a new service (recommended)" />
            <Radio value="same" label="Into this service (replaces its data)" />
          </RadioGroup>
          {into === "new" ? (
            <Field label="New service name" hint="Optional.">
              {(p) => <Input {...p} placeholder={`${instance.serviceName}-restored`} value={name} onChange={(e) => setName(e.target.value)} />}
            </Field>
          ) : (
            <Field label={`Type ${instance.serviceName} to confirm`}>
              {(p) => <Input {...p} autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />}
            </Field>
          )}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={close}>
              Cancel
            </Button>
            <Button type="submit" variant={into === "same" ? "danger" : "primary"} loading={restore.isPending} disabled={!valid}>
              Restore
            </Button>
          </div>
        </form>
      )}
    </Modal>
  );
}
