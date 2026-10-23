import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Plus, Trash, World } from "@/components/ui/icons";
import { Field, Input, Select } from "@/components/ui/Input";
import { Alert, Badge } from "@/components/ui/misc";
import { api, type Instance, type RestartPolicy, type ServiceConfig } from "@/lib/api";

function Section({ title, description, children }: { title: string; description?: ReactNode; children: ReactNode }) {
  return (
    <section className="grid grid-cols-[200px_1fr] gap-8 border-b border-border py-6 first:pt-0 last:border-b-0">
      <div>
        <h3 className="text-base font-semibold">{title}</h3>
        {description ? <p className="mt-1 text-sm text-muted-foreground">{description}</p> : null}
      </div>
      <div className="flex min-w-0 flex-col gap-4">{children}</div>
    </section>
  );
}

/** Splits a start command into argv. Quotes group words; nothing is ever run
 *  through a shell. */
export function splitArgs(s: string): string[] {
  const out: string[] = [];
  const re = /"((?:\\.|[^"\\])*)"|'([^']*)'|(\S+)/g;
  for (const m of s.matchAll(re)) out.push(m[1] !== undefined ? m[1].replace(/\\(.)/g, "$1") : (m[2] ?? m[3] ?? ""));
  return out;
}

export function joinArgs(a: string[] | undefined): string {
  return (a ?? []).map((x) => (/[\s"']/.test(x) || x === "" ? JSON.stringify(x) : x)).join(" ");
}

interface Form {
  image: string;
  startCommand: string;
  port: string;
  healthcheckPath: string;
  healthcheckTimeoutSecs: string;
  restartPolicy: RestartPolicy;
  memoryMb: string;
  cpuMillis: string;
}

const toForm = (c: ServiceConfig): Form => ({
  image: c.source.image,
  startCommand: joinArgs(c.startCommand),
  port: String(c.port),
  healthcheckPath: c.healthcheckPath ?? "",
  healthcheckTimeoutSecs: String(c.healthcheckTimeoutSecs),
  restartPolicy: c.restartPolicy,
  memoryMb: c.memoryMb ? String(c.memoryMb) : "",
  cpuMillis: c.cpuMillis ? String(c.cpuMillis) : "",
});

const optNum = (s: string) => (s.trim() ? Number(s) : undefined);

const fromForm = (f: Form): ServiceConfig => ({
  source: { type: "image", image: f.image.trim() },
  startCommand: f.startCommand.trim() ? splitArgs(f.startCommand) : undefined,
  port: Number(f.port),
  healthcheckPath: f.healthcheckPath.trim() || undefined,
  healthcheckTimeoutSecs: Number(f.healthcheckTimeoutSecs) || 300,
  restartPolicy: f.restartPolicy,
  memoryMb: optNum(f.memoryMb),
  cpuMillis: optNum(f.cpuMillis),
});

export function SettingsTab({
  instance,
  canEdit,
  canDelete,
  onDeleted,
}: {
  instance: Instance;
  canEdit: boolean;
  canDelete: boolean;
  onDeleted: () => void;
}) {
  const qc = useQueryClient();
  const [form, setForm] = useState(() => toForm(instance.config));
  useEffect(() => setForm(toForm(instance.config)), [instance.config]);
  const dirty = JSON.stringify(fromForm(form)) !== JSON.stringify(fromForm(toForm(instance.config)));
  const set = <K extends keyof Form>(k: K) => (e: { target: { value: string } }) => setForm((f) => ({ ...f, [k]: e.target.value as Form[K] }));

  const save = useMutation({
    mutationFn: () => api.updateInstance(instance.id, fromForm(form)),
    onSuccess: (i) => qc.setQueryData(["instance", instance.id], i),
  });
  const deploy = useMutation({
    mutationFn: () => api.deploy(instance.id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["deployments", instance.id] }),
  });
  const remove = useMutation({
    mutationFn: () => api.deleteService(instance.serviceId),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["project", instance.projectId] });
      onDeleted();
    },
  });

  return (
    <div className="flex flex-col pb-20">
      <Section title="Source" description="The image every deploy pulls.">
        <Field label="Image">{(p) => <Input {...p} mono disabled={!canEdit} value={form.image} onChange={set("image")} />}</Field>
      </Section>

      <Section title="Networking" description="Public hostnames on your apps domain. HTTPS is set up automatically.">
        <Domains instanceId={instance.id} canEdit={canEdit} />
        <Field label="Port" hint="The port your app listens on. It's also set as PORT.">
          {(p) => <Input {...p} className="w-32" inputMode="numeric" disabled={!canEdit} value={form.port} onChange={set("port")} />}
        </Field>
      </Section>

      <Section title="Deploy">
        <Field label="Start command" hint="Leave empty to use the image's default. Never run through a shell.">
          {(p) => <Input {...p} mono disabled={!canEdit} placeholder="node server.js" value={form.startCommand} onChange={set("startCommand")} />}
        </Field>
        <div className="grid grid-cols-[1fr_160px] gap-3">
          <Field label="Health check path" hint="A deploy goes live once this returns 2xx.">
            {(p) => <Input {...p} mono disabled={!canEdit} placeholder="/health" value={form.healthcheckPath} onChange={set("healthcheckPath")} />}
          </Field>
          <Field label="Timeout (seconds)">
            {(p) => <Input {...p} inputMode="numeric" disabled={!canEdit} value={form.healthcheckTimeoutSecs} onChange={set("healthcheckTimeoutSecs")} />}
          </Field>
        </div>
        <Field label="Restart policy">
          {(p) => (
            <Select {...p} className="w-56" disabled={!canEdit} value={form.restartPolicy} onChange={set("restartPolicy")}>
              <option value="on_failure">On failure</option>
              <option value="always">Always</option>
              <option value="never">Never</option>
            </Select>
          )}
        </Field>
      </Section>

      <Section title="Resources" description="Hard limits for the container. Empty means no limit.">
        <div className="grid grid-cols-2 gap-3">
          <Field label="Memory (MB)">
            {(p) => <Input {...p} inputMode="numeric" disabled={!canEdit} placeholder="No limit" value={form.memoryMb} onChange={set("memoryMb")} />}
          </Field>
          <Field label="CPU (millicores)" hint="1000 = one vCPU.">
            {(p) => <Input {...p} inputMode="numeric" disabled={!canEdit} placeholder="No limit" value={form.cpuMillis} onChange={set("cpuMillis")} />}
          </Field>
        </div>
      </Section>

      {canDelete ? (
        <Section title="Danger zone">
          {remove.error ? <Alert>{remove.error.message}</Alert> : null}
          <div className="flex items-center justify-between gap-4 rounded-lg border border-destructive/40 p-4">
            <div className="text-sm">
              <div className="font-medium">Delete service</div>
              <div className="text-muted-foreground">Removes it from every environment and stops its containers.</div>
            </div>
            <Button variant="danger" loading={remove.isPending} onClick={() => remove.mutate()}>
              Delete service
            </Button>
          </div>
        </Section>
      ) : null}

      {canEdit && (dirty || save.isSuccess || save.error || deploy.error) ? (
        <div className="sticky bottom-0 mt-4 flex items-center justify-between gap-3 rounded-lg border border-border bg-popover px-4 py-3 shadow-menu">
          <div className="min-w-0 text-sm">
            {save.error ? (
              <span className="text-error">{save.error.message}</span>
            ) : deploy.error ? (
              <span className="text-error">{deploy.error.message}</span>
            ) : dirty ? (
              "Unsaved changes"
            ) : (
              "Saved. Deploy to apply the new settings."
            )}
          </div>
          <div className="flex shrink-0 gap-2">
            {dirty ? (
              <>
                <Button size="sm" variant="ghost" onClick={() => setForm(toForm(instance.config))}>
                  Discard
                </Button>
                <Button size="sm" variant="secondary" loading={save.isPending} onClick={() => save.mutate()}>
                  Save
                </Button>
              </>
            ) : (
              <Button
                size="sm"
                variant="secondary"
                loading={deploy.isPending}
                onClick={() => deploy.mutate(undefined, { onSuccess: () => save.reset() })}
              >
                Deploy
              </Button>
            )}
          </div>
        </div>
      ) : null}
    </div>
  );
}

function Domains({ instanceId, canEdit }: { instanceId: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const key = ["domains", instanceId];
  const domains = useQuery({ queryKey: key, queryFn: () => api.domains(instanceId) });
  const invalidate = () => Promise.all([qc.invalidateQueries({ queryKey: key }), qc.invalidateQueries({ queryKey: ["project"] })]);
  const generate = useMutation({ mutationFn: () => api.generateDomain(instanceId), onSuccess: invalidate });
  const remove = useMutation({ mutationFn: (id: string) => api.deleteDomain(id), onSuccess: invalidate });
  const error = generate.error ?? remove.error;
  return (
    <div className="flex flex-col gap-2">
      <div className="text-sm font-medium">Public domains</div>
      {error ? <Alert>{error.message}</Alert> : null}
      {domains.data?.map((d) => (
        <div key={d.id} className="flex h-10 items-center gap-2 rounded-md border border-border pr-1 pl-3">
          <World className="size-4 text-muted-foreground" />
          <a href={`${d.hostname.endsWith(".localhost") ? "http" : "https"}://${d.hostname}`} target="_blank" rel="noreferrer" className="min-w-0 flex-1 truncate text-sm hover:underline">
            {d.hostname}
          </a>
          {d.kind === "generated" ? <Badge>Generated</Badge> : null}
          <CopyButton value={d.hostname} label="Copy domain" />
          {canEdit ? (
            <button aria-label={`Remove ${d.hostname}`} onClick={() => remove.mutate(d.id)} className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-error">
              <Trash className="size-4" />
            </button>
          ) : null}
        </div>
      ))}
      {canEdit ? (
        <div>
          <Button size="sm" variant="outline" icon={<Plus />} loading={generate.isPending} onClick={() => generate.mutate()}>
            Generate domain
          </Button>
        </div>
      ) : null}
    </div>
  );
}
