import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { api, type Environment, type ProjectDetail } from "@/lib/api";
import { branchOverrides, repoServices, serviceDeletion } from "./environments";
import { ConfirmDelete } from "./settings/SettingsParts";

const EMPTY = "";

/** New environment: a duplicate of one (services, settings, all variables) or
 *  an empty one, with the branch each repo service deploys from there. */
export function NewEnvironmentDialog({
  open,
  onOpenChange,
  project,
  current,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  project: ProjectDetail;
  /** Offered (and picked) first. */
  current?: Environment;
  onCreated: (id: string) => void;
}) {
  const qc = useQueryClient();
  const envs = project.environments;
  const [name, setName] = useState("");
  // An environment id to duplicate, or EMPTY; unset = the current one.
  const [from, setFrom] = useState<string>();
  const [branches, setBranches] = useState<Record<string, string>>({});
  const choice = from ?? current?.id ?? envs[0]?.id ?? EMPTY;
  // Services take their settings (and branch) from the duplicated environment, or from the default one.
  const source = choice === EMPTY ? envs.find((e) => e.isDefault) ?? envs[0] : envs.find((e) => e.id === choice);
  const repos = source ? repoServices(project, source.id) : [];
  const reset = () => {
    setName("");
    setFrom(undefined);
    setBranches({});
  };
  const create = useMutation({
    mutationFn: () =>
      api.createEnvironment(project.id, {
        name: name.trim(),
        ...(choice === EMPTY ? {} : { duplicateFrom: choice }),
        branches: branchOverrides(repos, branches),
      }),
    onSuccess: async (e) => {
      await qc.invalidateQueries({ queryKey: ["project", project.id] });
      onCreated(e.id);
      reset();
      onOpenChange(false);
    },
  });
  const ordered = current ? [current, ...envs.filter((e) => e.id !== current.id)] : envs;
  return (
    <Modal
      open={open}
      onOpenChange={(o) => {
        if (!o) create.reset();
        onOpenChange(o);
      }}
      title="New environment"
      description="Every service gets its own copy in the new environment, with its own variables, volumes and domains."
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          create.mutate();
        }}
      >
        {create.error ? <Alert>{create.error.message}</Alert> : null}
        <Field label="Name">
          {(p) => <Input {...p} autoFocus required maxLength={32} placeholder="staging" value={name} onChange={(e) => setName(e.target.value)} />}
        </Field>
        <Field
          label="Start from"
          hint={
            choice === EMPTY
              ? "Services get the default environment's settings, without variables."
              : "Copies every service's settings and variables; sealed ones stay sealed. Databases start empty, with new credentials; buckets aren't copied."
          }
        >
          {(p) => (
            <Select {...p} value={choice} onChange={(e) => setFrom(e.target.value)}>
              {ordered.map((e) => (
                <option key={e.id} value={e.id}>
                  Duplicate {e.name}
                </option>
              ))}
              <option value={EMPTY}>Empty</option>
            </Select>
          )}
        </Field>
        {repos.length > 0 ? (
          <fieldset className="flex flex-col gap-3">
            <legend className="mb-2 text-sm">Branches</legend>
            {repos.map((s) => (
              <div key={s.serviceId} className="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)] items-center gap-3">
                <label htmlFor={`branch-${s.serviceId}`} className="min-w-0">
                  <span className="block truncate text-sm">{s.name}</span>
                  <span className="block truncate font-mono text-xs text-muted-foreground">{s.repo}</span>
                </label>
                <Input
                  id={`branch-${s.serviceId}`}
                  mono
                  autoComplete="off"
                  spellCheck={false}
                  placeholder={s.branch}
                  value={branches[s.serviceId] ?? s.branch}
                  onChange={(e) => setBranches((b) => ({ ...b, [s.serviceId]: e.target.value }))}
                />
              </div>
            ))}
            <p className="text-sm text-muted-foreground">Pushes to a service's branch deploy it in this environment.</p>
          </fieldset>
        ) : null}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending} disabled={!name.trim()}>
            Create environment
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** A service to delete from one environment. */
export interface ServiceToDelete {
  serviceId: string;
  name: string;
  environmentId: string;
  /** Known by the caller (the service sheet); otherwise read from the project. */
  instanceId?: string;
  environmentName?: string;
  volume?: boolean;
}

/** Deleting a service from the environment being viewed (like Railway): only
 *  that environment's instance goes. Deleting it from every environment is a
 *  separate, typed confirmation. Deleting needs sudo; the API client opens the
 *  re-auth dialog and retries. */
export function DeleteServiceDialog({
  service,
  onClose,
  projectId,
  onDeleted,
}: {
  service?: ServiceToDelete;
  onClose: () => void;
  projectId: string;
  /** `everywhere`: the service itself is gone (not just its instance here). */
  onDeleted: (serviceId: string, everywhere: boolean) => void;
}) {
  const qc = useQueryClient();
  const [last, setLast] = useState(service);
  const [everywhere, setEverywhere] = useState(false);
  const [warnings, setWarnings] = useState<{ list: string[]; everywhere: boolean } | null>(null);
  // Callers may pass a fresh object each render: reset only for another service/environment.
  const key = service && `${service.serviceId}/${service.environmentId}`;
  const lastKey = last && `${last.serviceId}/${last.environmentId}`;
  if (service && key !== lastKey) {
    setLast(service);
    setEverywhere(false);
    setWarnings(null);
  }
  const project = qc.getQueryData<ProjectDetail>(["project", projectId]);
  const derived = last ? serviceDeletion(project, last.serviceId, last.environmentId) : undefined;
  const what = derived && {
    ...derived,
    instanceId: last?.instanceId ?? derived.instanceId,
    environmentName: last?.environmentName ?? derived.environmentName,
    volume: last?.volume ?? derived.volume,
  };
  const envName = what?.environmentName ?? "this environment";
  const others = (what?.environments ?? []).filter((e) => e !== what?.environmentName);
  const done = async (serviceId: string, gone: boolean, w: string[] | undefined) => {
    await Promise.all([
      qc.invalidateQueries({ queryKey: ["project", projectId] }),
      qc.invalidateQueries({ queryKey: ["pending"] }),
    ]);
    if (w?.length) {
      setWarnings({ list: w, everywhere: gone });
      return;
    }
    onDeleted(serviceId, gone);
    onClose();
  };
  const removeHere = useMutation({
    mutationFn: (instanceId: string) => api.deleteInstance(instanceId),
    onSuccess: (r) => last && done(last.serviceId, r.serviceDeleted, r.warnings),
  });
  const removeAll = useMutation({
    mutationFn: (id: string) => api.deleteServiceEverywhere(id),
    onSuccess: (r, id) => done(id, true, r.warnings),
  });
  const close = () => {
    if (warnings && last) onDeleted(last.serviceId, warnings.everywhere);
    removeHere.reset();
    removeAll.reset();
    onClose();
  };
  const name = last?.name ?? "service";
  const description = [
    `Stops its deployments in ${envName} and removes its variables, domains and settings there.`,
    what?.volume ? `Its volume data in ${envName} is deleted.` : null,
    what?.r2 ? `Its Cloudflare token for ${envName} is revoked; the bucket and its data stay in Cloudflare.` : null,
    others.length
      ? `${others.join(", ")} ${others.length === 1 ? "is" : "are"} not affected.`
      : "This is its only environment, so the service is deleted too.",
    "This can't be undone.",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <Modal
      open={!!service}
      onOpenChange={(o) => {
        if (!o) close();
      }}
      title={`Delete ${name} from ${envName}?`}
      description={description}
    >
      <div className="flex flex-col gap-4">
        {removeHere.error ? <Alert>{removeHere.error.message}</Alert> : null}
        {removeAll.error ? <Alert>{removeAll.error.message}</Alert> : null}
        {warnings ? (
          <>
            {warnings.list.map((w) => (
              <Alert key={w} tone="warn">
                {w}
              </Alert>
            ))}
            <div className="flex justify-end">
              <Button variant="secondary" onClick={close}>
                Done
              </Button>
            </div>
          </>
        ) : everywhere ? (
          <div className="flex flex-col gap-3 rounded-xl border border-destructive/40 p-4">
            <div className="text-sm">
              <div className="font-bold">Delete from all environments</div>
              <div className="text-muted-foreground">
                Removes {name} from {what?.environments.length ?? "every"} environment
                {what?.environments.length === 1 ? "" : "s"}
                {what?.environments.length ? ` (${what.environments.join(", ")})` : ""}: every deployment stops and
                every volume's data is deleted, production's included.
              </div>
            </div>
            <ConfirmDelete
              name={name}
              label="Delete from all environments"
              pending={removeAll.isPending}
              onConfirm={() => last && removeAll.mutate(last.serviceId)}
              onCancel={() => {
                setEverywhere(false);
                removeAll.reset();
              }}
            />
          </div>
        ) : (
          <>
            <div className="flex justify-end gap-2">
              <Button variant="ghost" onClick={close}>
                Cancel
              </Button>
              <Button
                variant="danger"
                loading={removeHere.isPending}
                disabled={!what?.instanceId}
                onClick={() => what?.instanceId && removeHere.mutate(what.instanceId)}
              >
                Delete from {envName}
              </Button>
            </div>
            {others.length ? (
              <div className="border-t border-border pt-3">
                <Button variant="link" size="sm" className="px-0 text-error" onClick={() => setEverywhere(true)}>
                  Delete from all environments…
                </Button>
              </div>
            ) : null}
          </>
        )}
      </div>
    </Modal>
  );
}
