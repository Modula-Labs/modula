import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Lock } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { api, type Instance, type ProjectDetail } from "@/lib/api";
import { BUCKET_VARIABLES, suggestBucketName } from "@/lib/templates";
import { R2ChoiceFields, r2Choice, type R2Mode } from "./BucketFields";
import { ConnectRow } from "./ConnectDialog";
import { DeleteServiceDialog } from "./ProjectDialogs";

function Section({
  title,
  description,
  children,
}: {
  title: string;
  description?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="grid gap-4 border-b border-border py-6 first:pt-0 last:border-b-0">
      <div>
        <h3 className="text-base font-semibold">{title}</h3>
        {description ? (
          <p className="mt-1 text-sm text-muted-foreground">{description}</p>
        ) : null}
      </div>
      <div className="flex min-w-0 flex-col gap-4">{children}</div>
    </section>
  );
}

/** A bucket service's own tab: where it lives, how to connect, and (for R2)
 *  creating, linking, unlinking or deleting this environment's bucket. */
export function BucketTab({
  instance,
  canEdit,
  canAdmin,
  onDeleted,
}: {
  instance: Instance;
  /** Member: create, link and unlink buckets; delete the service. */
  canEdit: boolean;
  /** Admin: delete the bucket at Cloudflare. */
  canAdmin: boolean;
  onDeleted: () => void;
}) {
  const r2 = instance.template === "r2";
  const [deleting, setDeleting] = useState(false);

  return (
    <div className="flex flex-col pb-20">
      {r2 ? (
        <R2Section instance={instance} canEdit={canEdit} canAdmin={canAdmin} />
      ) : (
        <SelfHostedSection instance={instance} />
      )}
      {!r2 || instance.r2 ? <ConnectSection instance={instance} /> : null}
      {canEdit ? (
        <Section title="Danger zone">
          <div className="flex items-center justify-between gap-4 rounded-lg border border-destructive/40 p-4">
            <div className="text-sm">
              <div className="font-medium">Delete service from {instance.environmentName}</div>
              <div className="text-muted-foreground">
                {r2
                  ? `Removes it from ${instance.environmentName} and revokes its Cloudflare token there. The bucket and its data stay in Cloudflare.`
                  : `Removes it from ${instance.environmentName}, stops its container there and deletes that volume with the bucket's data. Other environments keep theirs.`}
              </div>
            </div>
            <Button variant="danger" onClick={() => setDeleting(true)}>
              Delete service
            </Button>
          </div>
          <DeleteServiceDialog
        service={
          deleting
            ? {
                serviceId: instance.serviceId,
                name: instance.serviceName,
                environmentId: instance.environmentId,
                instanceId: instance.id,
                environmentName: instance.environmentName,
                volume: !!instance.volume,
              }
            : undefined
        }
        onClose={() => setDeleting(false)}
        projectId={instance.projectId}
        onDeleted={() => onDeleted()}
      />
        </Section>
      ) : null}
    </div>
  );
}

function SelfHostedSection({ instance }: { instance: Instance }) {
  return (
    <Section
      title="Self-hosted bucket"
      description="An S3-compatible server (rclone) running on your own servers. Its data lives on the service's volume."
    >
      <ul className="flex flex-col gap-1.5 text-sm text-muted-foreground">
        <li>
          Reachable from other services on the private network at{" "}
          <span className="font-mono text-foreground">
            {instance.serviceName}.modula.internal:9000
          </span>
          .
        </li>
        <li>
          Use path-style URLs (
          <span className="font-mono">S3_FORCE_PATH_STYLE</span> is{" "}
          <span className="font-mono">true</span>).
        </li>
        <li>
          The bucket is the volume at{" "}
          <span className="font-mono">
            {instance.volume?.mountPath ?? "/data/bucket"}
          </span>
          : keep <span className="font-mono">S3_BUCKET</span> as it is.
        </li>
      </ul>
    </Section>
  );
}

function R2Section({
  instance,
  canEdit,
  canAdmin,
}: {
  instance: Instance;
  canEdit: boolean;
  canAdmin: boolean;
}) {
  const qc = useQueryClient();
  const project = qc.getQueryData<ProjectDetail>([
    "project",
    instance.projectId,
  ]);
  const [mode, setMode] = useState<R2Mode>("new");
  const [bucket, setBucket] = useState(() =>
    suggestBucketName(instance.serviceName, instance.environmentName),
  );
  const [existing, setExisting] = useState("");
  const [deleting, setDeleting] = useState(false);
  const [unlinkWarnings, setUnlinkWarnings] = useState<string[]>([]);
  const refresh = async (i?: Instance) => {
    if (i) qc.setQueryData(["instance", instance.id], i);
    else await qc.invalidateQueries({ queryKey: ["instance", instance.id] });
    await Promise.all([
      qc.invalidateQueries({ queryKey: ["project", instance.projectId] }),
      qc.invalidateQueries({ queryKey: ["variables", instance.id] }),
      qc.invalidateQueries({ queryKey: ["cloudflare-r2-buckets"] }),
    ]);
  };
  const choice = r2Choice(mode, bucket, existing);
  const provision = useMutation({
    mutationFn: () =>
      api.provisionBucket(instance.id, choice ?? { bucket: "" }),
    onSuccess: (i) => refresh(i),
  });
  const unlink = useMutation({
    mutationFn: () => api.unlinkBucket(instance.id),
    onSuccess: async (r) => {
      setUnlinkWarnings(r.warnings ?? []);
      await refresh();
    },
  });
  const link = instance.r2;

  if (!link) {
    return (
      <Section
        title="Cloudflare R2"
        description={`${instance.environmentName} has no bucket yet. Create one, or link a bucket already in your Cloudflare account.`}
      >
        {unlinkWarnings.map((w) => (
          <Alert key={w} tone="warn">
            {w}
          </Alert>
        ))}
        {canEdit ? (
          <>
            {provision.error ? <Alert>{provision.error.message}</Alert> : null}
            <R2ChoiceFields
              workspaceId={project?.workspaceId}
              mode={mode}
              onMode={setMode}
              bucket={bucket}
              onBucket={setBucket}
              existing={existing}
              onExisting={setExisting}
            />
            <div>
              <Button
                variant="secondary"
                loading={provision.isPending}
                disabled={!choice}
                onClick={() => provision.mutate()}
              >
                {mode === "new" ? "Create bucket" : "Link bucket"}
              </Button>
            </div>
          </>
        ) : (
          <p className="text-sm text-muted-foreground">
            Ask a member to add one.
          </p>
        )}
      </Section>
    );
  }

  return (
    <Section
      title="Cloudflare R2"
      description="Its credentials can read and write this bucket's objects, and nothing else in the account."
    >
      <div className="flex flex-col gap-2 rounded-lg border border-border p-4 text-sm">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-mono text-[13px] font-medium">
            {link.bucket}
          </span>
          <Badge tone="neutral">
            {link.created ? "Created here" : "Linked"}
          </Badge>
        </div>
        <div className="truncate font-mono text-xs text-muted-foreground">
          https://{link.accountId}.r2.cloudflarestorage.com
        </div>
      </div>
      {unlink.error ? <Alert>{unlink.error.message}</Alert> : null}
      {canEdit ? (
        <div className="flex flex-wrap gap-2">
          <Button
            variant="secondary"
            loading={unlink.isPending}
            onClick={() => unlink.mutate()}
          >
            Unlink bucket
          </Button>
          {link.created && canAdmin ? (
            <Button variant="danger-outline" onClick={() => setDeleting(true)}>
              Delete bucket…
            </Button>
          ) : null}
        </div>
      ) : null}
      <p className="text-xs text-muted-foreground">
        Unlinking revokes the credentials and removes the variables; the bucket
        and its data stay in Cloudflare.
        {link.created
          ? ""
          : " A linked bucket can only be deleted in the Cloudflare dashboard."}
      </p>
      <DeleteBucketDialog
        open={deleting}
        onOpenChange={setDeleting}
        instance={instance}
        bucket={link.bucket}
        onDeleted={() => refresh()}
      />
    </Section>
  );
}

function DeleteBucketDialog({
  open,
  onOpenChange,
  instance,
  bucket,
  onDeleted,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  instance: Instance;
  bucket: string;
  onDeleted: () => Promise<void>;
}) {
  const [typed, setTyped] = useState("");
  const del = useMutation({
    mutationFn: () => api.deleteBucket(instance.id, typed.trim()),
    onSuccess: async () => {
      await onDeleted();
      onOpenChange(false);
      setTyped("");
    },
  });
  return (
    <Modal
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) {
          setTyped("");
          del.reset();
        }
      }}
      title={`Delete ${bucket}?`}
      description="The bucket is deleted at Cloudflare and its credentials revoked. Cloudflare only deletes empty buckets: empty it first. This can't be undone."
      footer={
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            variant="danger"
            loading={del.isPending}
            disabled={typed.trim() !== bucket}
            onClick={() => del.mutate()}
          >
            Delete bucket
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-3">
        {del.error ? <Alert>{del.error.message}</Alert> : null}
        <Field label={`Type ${bucket} to confirm`}>
          {(p) => (
            <Input
              {...p}
              mono
              autoComplete="off"
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
            />
          )}
        </Field>
      </div>
    </Modal>
  );
}

/** The S3 variables, as references other services use (values revealable
 *  unless sealed). */
function ConnectSection({ instance }: { instance: Instance }) {
  const vars = useQuery({
    queryKey: ["variables", instance.id],
    queryFn: () => api.variables(instance.id),
  });
  return (
    <Section
      title="Connect"
      description={
        <>
          Reference these from other services, e.g.{" "}
          <span className="font-mono">{`\${{${instance.serviceName}.S3_BUCKET}}`}</span>
          . <span className="font-mono">BUCKET_NAME</span> equals{" "}
          <span className="font-mono">S3_BUCKET</span>, and{" "}
          <span className="font-mono">S3_FORCE_PATH_STYLE</span> is{" "}
          <span className="font-mono">true</span>.
        </>
      }
    >
      {vars.isPending ? (
        <Skeleton className="h-40" />
      ) : vars.error ? (
        <Alert>{vars.error.message}</Alert>
      ) : (
        <div className="divide-y divide-border rounded-lg border border-border">
          {BUCKET_VARIABLES.map((e) => {
            const info = vars.data.find((v) => v.name === e.variable);
            const reference = `\${{${instance.serviceName}.${e.variable}}}`;
            return info?.sealed ? (
              <div
                key={e.variable}
                className="flex flex-col gap-1.5 px-3 py-2.5"
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="text-sm font-medium">{e.label}</span>
                  <span className="truncate font-mono text-xs text-muted-foreground">
                    {reference}
                  </span>
                </div>
                <div className="flex items-center gap-2 text-xs text-muted-foreground [&_svg]:size-3.5">
                  <Lock aria-hidden />
                  Sealed: services can reference it, but it can't be shown.
                </div>
              </div>
            ) : (
              <ConnectRow
                key={e.variable}
                label={e.label}
                variable={e.variable}
                reference={reference}
                info={info}
              />
            );
          })}
        </div>
      )}
    </Section>
  );
}
