import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Card, CardContent, CardHeader } from "@/components/ui/Card";
import { Checkbox } from "@/components/ui/Controls";
import { Archive, Plus, Trash } from "@/components/ui/icons";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import {
  ApiError,
  api,
  type BackupProvider,
  type BackupTarget,
} from "@/lib/api";
import {
  PROVIDERS,
  defaultRegion,
  endpointHint,
  hourLabel,
} from "@/lib/backup";
import { UseR2Button } from "./settings/Integrations";

const providerLabel = (p: BackupProvider) =>
  PROVIDERS.find((x) => x.value === p)?.label ?? p;

/** Backup targets (S3-compatible buckets) for a workspace. */
export function BackupStorage({
  workspaceId,
  canEdit,
}: {
  workspaceId: string;
  canEdit: boolean;
}) {
  const qc = useQueryClient();
  const key = ["backup-targets", workspaceId];
  const targets = useQuery({
    queryKey: key,
    queryFn: () => api.backupTargets(workspaceId),
    retry: (n, e) => !(e instanceof ApiError && e.status === 404) && n < 2,
  });
  const [adding, setAdding] = useState(false);
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteBackupTarget(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: key }),
  });

  if (targets.error instanceof ApiError && targets.error.status === 404) {
    return <Alert tone="warn">This server doesn't support backups yet.</Alert>;
  }
  return (
    <>
      {remove.error ? <Alert>{remove.error.message}</Alert> : null}
      {targets.isPending ? (
        <Skeleton className="h-16" />
      ) : targets.error ? (
        <Alert>{targets.error.message}</Alert>
      ) : targets.data.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No storage yet. Add a bucket to back up your databases.
        </p>
      ) : (
        <div className="divide-y divide-border rounded-xl border border-border">
          {targets.data.map((t) => (
            <TargetRow
              key={t.id}
              t={t}
              canEdit={canEdit}
              onDelete={() => remove.mutate(t.id)}
              busy={remove.isPending}
            />
          ))}
        </div>
      )}
      {canEdit ? (
        <div className="flex gap-2">
          <Button
            variant="secondary"
            icon={<Plus />}
            onClick={() => setAdding(true)}
          >
            Add storage
          </Button>
          <UseR2Button workspaceId={workspaceId} />
        </div>
      ) : null}
      <AddTargetDialog
        workspaceId={workspaceId}
        open={adding}
        onOpenChange={setAdding}
      />
    </>
  );
}

function TargetRow({
  t,
  canEdit,
  onDelete,
  busy,
}: {
  t: BackupTarget;
  canEdit: boolean;
  onDelete: () => void;
  busy: boolean;
}) {
  return (
    <div className="flex min-h-14 items-center gap-3 px-3 py-2.5 hover:bg-muted/50">
      <Archive className="size-5 text-muted-foreground" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm">{t.name}</span>
          <Badge>{providerLabel(t.provider)}</Badge>
        </div>
        <div className="truncate font-mono text-[13px] text-muted-foreground">
          {t.bucket}
          {t.prefix ? `/${t.prefix}` : ""} · {t.endpoint}
        </div>
      </div>
      {canEdit ? (
        <Button
          variant="ghost"
          size="icon"
          aria-label={`Delete ${t.name}`}
          disabled={busy}
          onClick={onDelete}
        >
          <Trash />
        </Button>
      ) : null}
    </div>
  );
}

function AddTargetDialog({
  workspaceId,
  open,
  onOpenChange,
}: {
  workspaceId: string;
  open: boolean;
  onOpenChange: (o: boolean) => void;
}) {
  const qc = useQueryClient();
  const blank = {
    name: "",
    provider: "s3" as BackupProvider,
    endpoint: "",
    region: "",
    bucket: "",
    prefix: "",
    accessKeyId: "",
    secretAccessKey: "",
  };
  const [f, setF] = useState(blank);
  const set =
    <K extends keyof typeof blank>(k: K) =>
    (e: { target: { value: string } }) =>
      setF((x) => ({ ...x, [k]: e.target.value as (typeof blank)[K] }));
  const create = useMutation({
    mutationFn: () =>
      api.createBackupTarget(workspaceId, {
        name: f.name.trim(),
        provider: f.provider,
        endpoint: f.endpoint.trim(),
        region: f.region.trim(),
        bucket: f.bucket.trim(),
        prefix: f.prefix.trim() || undefined,
        accessKeyId: f.accessKeyId.trim(),
        secretAccessKey: f.secretAccessKey,
      }),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["backup-targets", workspaceId] });
      onOpenChange(false);
    },
  });
  const unfilled = f.endpoint.includes("<");
  const valid =
    f.name.trim() &&
    f.endpoint.trim() &&
    !unfilled &&
    f.bucket.trim() &&
    f.accessKeyId.trim() &&
    f.secretAccessKey;

  // Starting over when the dialog closes keeps secrets out of memory longer than needed.
  useEffect(() => {
    if (!open) {
      setF(blank);
      create.reset();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      width={520}
      title="Add backup storage"
      description="Any S3-compatible bucket. Modula writes a small test object to check the credentials."
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) create.mutate();
        }}
      >
        {create.error ? <Alert>{create.error.message}</Alert> : null}
        <div className="grid grid-cols-2 gap-3">
          <Field label="Name">
            {(p) => (
              <Input
                {...p}
                autoFocus
                placeholder="Offsite backups"
                value={f.name}
                onChange={set("name")}
              />
            )}
          </Field>
          <Field label="Provider">
            {(p) => (
              <Select
                {...p}
                value={f.provider}
                onChange={(e) => {
                  const provider = e.target.value as BackupProvider;
                  // Replace the endpoint only if the user hasn't typed their own.
                  const untouched =
                    !f.endpoint ||
                    f.endpoint === endpointHint(f.provider, f.region);
                  setF((x) => ({
                    ...x,
                    provider,
                    region: x.region || defaultRegion(provider),
                    endpoint: untouched
                      ? endpointHint(provider, x.region)
                      : x.endpoint,
                  }));
                }}
              >
                {PROVIDERS.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        </div>
        <Field
          label="Endpoint"
          hint="Replace the <placeholders> with your own values."
          error={unfilled ? "Fill in the placeholder in the endpoint" : null}
        >
          {(p) => (
            <Input
              {...p}
              mono
              placeholder={endpointHint(f.provider, f.region)}
              value={f.endpoint}
              onChange={set("endpoint")}
            />
          )}
        </Field>
        <div className="grid grid-cols-3 gap-3">
          <Field label="Region">
            {(p) => (
              <Input
                {...p}
                placeholder={f.provider === "r2" ? "auto" : "us-east-1"}
                value={f.region}
                onChange={set("region")}
              />
            )}
          </Field>
          <Field label="Bucket">
            {(p) => (
              <Input {...p} mono value={f.bucket} onChange={set("bucket")} />
            )}
          </Field>
          <Field label="Prefix" hint="Optional.">
            {(p) => (
              <Input
                {...p}
                mono
                placeholder="modula/"
                value={f.prefix}
                onChange={set("prefix")}
              />
            )}
          </Field>
        </div>
        <Field label="Access key ID">
          {(p) => (
            <Input
              {...p}
              mono
              autoComplete="off"
              value={f.accessKeyId}
              onChange={set("accessKeyId")}
            />
          )}
        </Field>
        <Field
          label="Secret access key"
          hint="Stored encrypted. It's never shown again."
        >
          {(p) => (
            <Input
              {...p}
              mono
              type="password"
              autoComplete="new-password"
              value={f.secretAccessKey}
              onChange={set("secretAccessKey")}
            />
          )}
        </Field>
        <div className="flex justify-end gap-2">
          <Button
            type="button"
            variant="ghost"
            onClick={() => onOpenChange(false)}
          >
            Cancel
          </Button>
          <Button
            type="submit"
            variant="primary"
            loading={create.isPending}
            disabled={!valid}
          >
            Add storage
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** Instance admins: a daily backup of Modula's own database. */
export function SelfBackupCard({
  workspaceId,
}: {
  workspaceId: string | undefined;
}) {
  const qc = useQueryClient();
  const cfg = useQuery({
    queryKey: ["self-backup"],
    queryFn: api.selfBackup,
    retry: (n, e) => !(e instanceof ApiError && e.status === 404) && n < 2,
  });
  const targets = useQuery({
    queryKey: ["backup-targets", workspaceId],
    queryFn: () => api.backupTargets(workspaceId!),
    enabled: !!workspaceId,
  });
  const [form, setForm] = useState({
    enabled: false,
    targetId: "",
    hourUtc: 3,
  });
  useEffect(() => {
    if (cfg.data)
      setForm({
        enabled: cfg.data.enabled,
        targetId: cfg.data.targetId ?? "",
        hourUtc: cfg.data.hourUtc,
      });
  }, [cfg.data]);
  const save = useMutation({
    mutationFn: () =>
      api.saveSelfBackup({ ...form, targetId: form.targetId || null }),
    onSuccess: (c) => qc.setQueryData(["self-backup"], c),
  });

  if (cfg.error instanceof ApiError && cfg.error.status === 404) return null;
  const dirty =
    !cfg.data ||
    cfg.data.enabled !== form.enabled ||
    (cfg.data.targetId ?? "") !== form.targetId ||
    cfg.data.hourUtc !== form.hourUtc;
  const known =
    !form.targetId || targets.data?.some((t) => t.id === form.targetId);

  return (
    <div className="mt-6">
      <Card>
        <CardHeader
          title={<span className="font-bold">Control-plane backup</span>}
          description="A daily backup of this instance's own database (projects, services, encrypted variables), written to one of your backup storages."
        />
        <CardContent>
          {cfg.isPending ? (
            <Skeleton className="h-24" />
          ) : cfg.error ? (
            <Alert>{cfg.error.message}</Alert>
          ) : (
            <div className="flex flex-col gap-4">
              {save.error ? <Alert>{save.error.message}</Alert> : null}
              <Checkbox
                checked={form.enabled}
                onCheckedChange={(enabled) =>
                  setForm((f) => ({ ...f, enabled }))
                }
                label="Back up this instance daily"
              />
              <div className="grid grid-cols-2 gap-3">
                <Field
                  label="Storage"
                  hint={
                    targets.data?.length === 0
                      ? "Add storage in the workspace settings first."
                      : "From the current workspace."
                  }
                >
                  {(p) => (
                    <Select
                      {...p}
                      value={form.targetId}
                      onChange={(e) =>
                        setForm((f) => ({ ...f, targetId: e.target.value }))
                      }
                    >
                      <option value="">Choose storage</option>
                      {!known ? (
                        <option value={form.targetId}>
                          Storage in another workspace
                        </option>
                      ) : null}
                      {targets.data?.map((t) => (
                        <option key={t.id} value={t.id}>
                          {t.name}
                        </option>
                      ))}
                    </Select>
                  )}
                </Field>
                <Field label="Time of day">
                  {(p) => (
                    <Select
                      {...p}
                      value={form.hourUtc}
                      onChange={(e) =>
                        setForm((f) => ({
                          ...f,
                          hourUtc: Number(e.target.value),
                        }))
                      }
                    >
                      {Array.from({ length: 24 }, (_, h) => (
                        <option key={h} value={h}>
                          {hourLabel(h)}
                        </option>
                      ))}
                    </Select>
                  )}
                </Field>
              </div>
              <div>
                <Button
                  variant="secondary"
                  loading={save.isPending}
                  disabled={!dirty || (form.enabled && !form.targetId)}
                  onClick={() => save.mutate()}
                >
                  Save
                </Button>
              </div>
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
