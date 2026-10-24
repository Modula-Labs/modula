import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Checkbox } from "@/components/ui/Controls";
import { Archive, DotsVertical, History, Trash } from "@/components/ui/icons";
import { Field, Input, Select } from "@/components/ui/Input";
import { Menu, MenuItem } from "@/components/ui/Menu";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, EmptyState, Skeleton, type Tone } from "@/components/ui/misc";
import { api, type Backup, type BackupSchedule, type Instance } from "@/lib/api";
import { hourLabel } from "@/lib/backup";
import { formatBytes, relativeTime } from "@/lib/format";

const DEFAULT_SCHEDULE: BackupSchedule = {
  enabled: true,
  targetId: "",
  hourUtc: 3,
  retention: { dailyDays: 6, weeklyDays: 27, monthlyDays: 89 },
};

const statusTone: Record<Backup["status"], Tone> = { running: "info", completed: "success", failed: "danger", deleted: "neutral" };
const statusLabel: Record<Backup["status"], string> = { running: "Running", completed: "Completed", failed: "Failed", deleted: "Deleted" };

function duration(b: Backup): string | null {
  if (!b.finishedAt) return null;
  const s = Math.max(0, Math.round((Date.parse(b.finishedAt) - Date.parse(b.startedAt)) / 1000));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${s % 60}s`;
}

export function BackupsTab({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const qc = useQueryClient();
  const key = ["backups", instance.id];
  const project = useQuery({ queryKey: ["project", instance.projectId], queryFn: () => api.project(instance.projectId) });
  const workspaceId = project.data?.workspaceId;
  const targets = useQuery({
    queryKey: ["backup-targets", workspaceId],
    queryFn: () => api.backupTargets(workspaceId!),
    enabled: !!workspaceId,
  });
  const info = useQuery({
    queryKey: key,
    queryFn: () => api.backups(instance.id),
    refetchInterval: (q) => (q.state.data?.backups.some((b) => b.status === "running") ? 3000 : false),
  });
  const refresh = () => qc.invalidateQueries({ queryKey: key });

  const [form, setForm] = useState<BackupSchedule>(DEFAULT_SCHEDULE);
  const saved = info.data?.schedule;
  useEffect(() => {
    setForm(saved ?? { ...DEFAULT_SCHEDULE, enabled: false });
  }, [saved]);
  const targetId = form.targetId || targets.data?.[0]?.id || "";
  const body: BackupSchedule = { ...form, targetId };
  const dirty = JSON.stringify(body) !== JSON.stringify(saved ?? { ...DEFAULT_SCHEDULE, enabled: false, targetId });

  const save = useMutation({ mutationFn: () => api.saveBackupSchedule(instance.id, body), onSuccess: refresh });
  const now = useMutation({ mutationFn: () => api.backupNow(instance.id), onSuccess: refresh });
  const remove = useMutation({ mutationFn: (id: string) => api.deleteBackup(id), onSuccess: refresh });
  const [restoring, setRestoring] = useState<Backup | null>(null);

  if (info.isPending || targets.isPending) return <Skeleton className="h-48" />;
  if (info.error) return <Alert tone="danger">{info.error.message}</Alert>;
  if (!targets.data?.length) {
    return (
      <EmptyState
        icon={<Archive />}
        title="No backup storage yet"
        body="Backups are written to an S3-compatible bucket you own. Add one in the workspace settings first."
        action={
          <Link to="/settings" className="text-sm underline">
            Open workspace settings
          </Link>
        }
      />
    );
  }

  const setRetention = (k: keyof BackupSchedule["retention"]) => (e: { target: { value: string } }) =>
    setForm((f) => ({ ...f, retention: { ...f.retention, [k]: Math.max(0, Math.floor(Number(e.target.value)) || 0) } }));
  const error = save.error ?? now.error ?? remove.error;

  return (
    <div className="flex flex-col gap-8 pb-10">
      <section className="flex flex-col gap-4">
        <div className="flex items-start justify-between gap-4">
          <div>
            <h3 className="text-base leading-6 font-bold">Schedule</h3>
            <p className="mt-1 text-sm text-muted-foreground">A daily backup to your storage. Older ones are pruned by the retention below.</p>
          </div>
          {canEdit ? (
            <Button variant="secondary" loading={now.isPending} onClick={() => now.mutate()}>
              Back up now
            </Button>
          ) : null}
        </div>
        {error ? <Alert>{error.message}</Alert> : null}
        <Checkbox
          disabled={!canEdit}
          checked={form.enabled}
          onCheckedChange={(enabled) => setForm((f) => ({ ...f, enabled }))}
          label="Run backups on a schedule"
        />
        <div className="grid grid-cols-2 gap-3">
          <Field label="Storage">
            {(p) => (
              <Select {...p} disabled={!canEdit} value={targetId} onChange={(e) => setForm((f) => ({ ...f, targetId: e.target.value }))}>
                {targets.data.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <Field label="Time of day">
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
        <div className="grid grid-cols-3 gap-3">
          <Field label="Keep daily (days)">
            {(p) => <Input {...p} type="number" min={0} disabled={!canEdit} value={form.retention.dailyDays} onChange={setRetention("dailyDays")} />}
          </Field>
          <Field label="Keep weekly (days)">
            {(p) => <Input {...p} type="number" min={0} disabled={!canEdit} value={form.retention.weeklyDays} onChange={setRetention("weeklyDays")} />}
          </Field>
          <Field label="Keep monthly (days)">
            {(p) => <Input {...p} type="number" min={0} disabled={!canEdit} value={form.retention.monthlyDays} onChange={setRetention("monthlyDays")} />}
          </Field>
        </div>
        {canEdit ? (
          <div>
            <Button variant="secondary" loading={save.isPending} disabled={!dirty || !targetId} onClick={() => save.mutate()}>
              Save schedule
            </Button>
          </div>
        ) : null}
      </section>

      <section className="flex flex-col gap-3">
        <h3 className="text-base leading-6 font-bold">Backups</h3>
        {info.data.backups.length === 0 ? (
          <p className="text-sm text-muted-foreground">No backups yet.</p>
        ) : (
          <div className="divide-y divide-border overflow-hidden rounded-xl border border-border">
            {info.data.backups.map((b) => (
              <div key={b.id} className="flex min-h-14 items-center gap-3 px-3 py-2.5 transition-colors hover:bg-muted/50">
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <Badge tone={statusTone[b.status]}>{statusLabel[b.status]}</Badge>
                    <span className="text-sm">{b.kind === "manual" ? "Manual" : "Scheduled"}</span>
                    <span className="text-[13px] text-muted-foreground">to {b.targetName}</span>
                  </div>
                  <div className="mt-0.5 text-[13px] text-muted-foreground">
                    <span title={new Date(b.startedAt).toLocaleString()}>{relativeTime(b.startedAt)}</span>
                    {duration(b) ? ` · took ${duration(b)}` : null}
                  </div>
                  {b.error ? <div className="mt-1 text-[13px] break-words text-error">{b.error}</div> : null}
                </div>
                <span className="text-sm text-muted-foreground tabular-nums">{formatBytes(b.sizeBytes)}</span>
                {canEdit && b.status !== "running" ? (
                  <Menu
                    trigger={
                      <Button aria-label="Backup actions" variant="ghost" size="icon-sm" className="text-muted-foreground">
                        <DotsVertical />
                      </Button>
                    }
                  >
                    {b.status === "completed" ? (
                      <MenuItem icon={<History />} onSelect={() => setRestoring(b)}>
                        Restore
                      </MenuItem>
                    ) : null}
                    {b.status !== "deleted" ? (
                      <MenuItem danger icon={<Trash />} onSelect={() => remove.mutate(b.id)}>
                        Delete
                      </MenuItem>
                    ) : null}
                  </Menu>
                ) : null}
              </div>
            ))}
          </div>
        )}
      </section>

      <RestoreDialog instance={instance} backup={restoring} onClose={() => setRestoring(null)} />
    </div>
  );
}

function RestoreDialog({ instance, backup, onClose }: { instance: Instance; backup: Backup | null; onClose: () => void }) {
  const qc = useQueryClient();
  const [typed, setTyped] = useState("");
  const restore = useMutation({
    mutationFn: () => api.restoreBackup(backup!.id),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["backups", instance.id] });
      void qc.invalidateQueries({ queryKey: ["deployments", instance.id] });
      onClose();
    },
  });
  return (
    <Modal
      open={!!backup}
      onOpenChange={(o) => {
        if (!o) {
          onClose();
          setTyped("");
          restore.reset();
        }
      }}
      title="Restore backup"
      description={`This replaces all current data in ${instance.serviceName} with the backup from ${backup ? new Date(backup.startedAt).toLocaleString() : ""}. Changes made since then are lost.`}
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (typed === instance.serviceName) restore.mutate();
        }}
      >
        {restore.error ? <Alert>{restore.error.message}</Alert> : null}
        <Field label={`Type ${instance.serviceName} to confirm`}>
          {(p) => <Input {...p} autoFocus autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />}
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="danger" loading={restore.isPending} disabled={typed !== instance.serviceName}>
            Restore
          </Button>
        </div>
      </form>
    </Modal>
  );
}
