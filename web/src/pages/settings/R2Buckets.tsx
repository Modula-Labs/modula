import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Skeleton } from "@/components/ui/misc";
import { api } from "@/lib/api";
import type { R2Bucket } from "@/lib/integrations";
import { uniqueName } from "@/lib/templates";
import { useR2Buckets } from "@/pages/project/BucketFields";

/** The Cloudflare account's R2 buckets, what uses each, and linking one into
 *  a project (it gets its own bucket-scoped credentials). */
export function R2Buckets({ workspaceId }: { workspaceId: string }) {
  const buckets = useR2Buckets(workspaceId);
  const [linking, setLinking] = useState<R2Bucket | null>(null);
  if (buckets.isPending) return <Skeleton className="ml-8 h-10" />;
  if (buckets.error)
    return <Alert tone="warn">R2: {buckets.error.message}</Alert>;
  const list = buckets.data.buckets;
  return (
    <div className="flex flex-col gap-2 pl-8">
      <div className="text-caps text-muted-foreground">R2 buckets</div>
      {list.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No buckets yet. Add one on a project&apos;s canvas: Add → Bucket.
        </p>
      ) : (
        <div className="divide-y divide-border rounded-lg border border-border">
          {list.map((b) => (
            <div
              key={b.name}
              className="flex flex-wrap items-center gap-3 px-3 py-2"
            >
              <div className="min-w-0 flex-1">
                <div className="truncate font-mono text-[13px]">{b.name}</div>
                <div className="truncate text-xs text-muted-foreground">
                  {b.links.length
                    ? b.links.map((l, n) => (
                        <span key={`${l.serviceId}-${l.environmentId}`}>
                          {n ? ", " : "Used by "}
                          <Link
                            to="/projects/$projectId"
                            params={{ projectId: l.projectId }}
                            className="hover:text-foreground hover:underline"
                          >
                            {l.projectName}/{l.serviceName}
                          </Link>{" "}
                          ({l.environmentName})
                        </span>
                      ))
                    : "Not used by any project"}
                  {b.location ? ` · ${b.location}` : ""}
                </div>
              </div>
              <Button size="sm" variant="ghost" onClick={() => setLinking(b)}>
                Link to a project
              </Button>
            </div>
          ))}
        </div>
      )}
      {linking ? (
        <LinkDialog
          workspaceId={workspaceId}
          bucket={linking}
          onClose={() => setLinking(null)}
        />
      ) : null}
    </div>
  );
}

function LinkDialog({
  workspaceId,
  bucket,
  onClose,
}: {
  workspaceId: string;
  bucket: R2Bucket;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const projects = useQuery({
    queryKey: ["projects", workspaceId],
    queryFn: () => api.projects(workspaceId),
  });
  const [projectId, setProjectId] = useState("");
  const project = useQuery({
    queryKey: ["project", projectId],
    queryFn: () => api.project(projectId),
    enabled: !!projectId,
  });
  const [environmentId, setEnvironmentId] = useState("");
  const [name, setName] = useState("");
  const taken = project.data?.services.map((s) => s.name) ?? [];
  // Bucket names are valid service names once they start with a letter.
  const service =
    name ||
    uniqueName(
      /^[a-z]/.test(bucket.name) ? bucket.name : `r2-${bucket.name}`,
      taken,
    );
  const env = environmentId || project.data?.environments[0]?.id || "";
  const link = useMutation({
    mutationFn: () =>
      api.createBucket(projectId, {
        name: service,
        environmentId: env,
        existing: bucket.name,
      }),
    onSuccess: async () => {
      await Promise.all([
        qc.invalidateQueries({ queryKey: ["project", projectId] }),
        qc.invalidateQueries({
          queryKey: ["cloudflare-r2-buckets", workspaceId],
        }),
      ]);
      onClose();
    },
  });
  return (
    <Modal
      open
      onOpenChange={(o) => !o && onClose()}
      title={`Link ${bucket.name}`}
      description="Adds a bucket service to the project with its own credentials, limited to this bucket. The bucket's data stays where it is."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="primary"
            loading={link.isPending}
            disabled={!projectId || !env}
            onClick={() => link.mutate()}
          >
            Link bucket
          </Button>
        </>
      }
    >
      <div className="flex flex-col gap-4">
        {link.error ? <Alert>{link.error.message}</Alert> : null}
        <Field label="Project">
          {(p) => (
            <Select
              {...p}
              value={projectId}
              onChange={(e) => {
                setProjectId(e.target.value);
                setEnvironmentId("");
              }}
            >
              <option value="">
                {projects.isPending ? "Loading…" : "Choose a project…"}
              </option>
              {projects.data?.map((pr) => (
                <option key={pr.id} value={pr.id}>
                  {pr.name}
                </option>
              ))}
            </Select>
          )}
        </Field>
        {project.data ? (
          <div className="grid grid-cols-2 gap-3">
            <Field label="Environment">
              {(p) => (
                <Select
                  {...p}
                  value={env}
                  onChange={(e) => setEnvironmentId(e.target.value)}
                >
                  {project.data.environments.map((e) => (
                    <option key={e.id} value={e.id}>
                      {e.name}
                    </option>
                  ))}
                </Select>
              )}
            </Field>
            <Field label="Service name">
              {(p) => (
                <Input
                  {...p}
                  placeholder={service}
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                />
              )}
            </Field>
          </div>
        ) : null}
      </div>
    </Modal>
  );
}
