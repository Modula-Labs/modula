import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { api, type Environment, type ProjectDetail } from "@/lib/api";
import { branchOverrides, repoServices } from "./environments";

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
              : "Copies every service's settings and variables; sealed ones stay sealed. Databases start empty, with new credentials."
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

/** Deleting needs sudo; the API client opens the re-auth dialog and retries. */
export function DeleteServiceDialog({
  service,
  onClose,
  projectId,
  onDeleted,
}: {
  service?: { id: string; name: string };
  onClose: () => void;
  projectId: string;
  onDeleted: (id: string) => void;
}) {
  const qc = useQueryClient();
  const [last, setLast] = useState(service);
  if (service && service !== last) setLast(service);
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteService(id),
    onSuccess: async (_, id) => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onDeleted(id);
      onClose();
    },
  });
  return (
    <Modal
      open={!!service}
      onOpenChange={(o) => {
        if (!o) {
          remove.reset();
          onClose();
        }
      }}
      title={`Delete ${last?.name ?? "service"}?`}
      description="Removes it from every environment with its variables, domains, volumes and deployments, and stops its containers. This can't be undone."
    >
      <div className="flex flex-col gap-4">
        {remove.error ? <Alert>{remove.error.message}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="danger" loading={remove.isPending} onClick={() => last && remove.mutate(last.id)}>
            Delete service
          </Button>
        </div>
      </div>
    </Modal>
  );
}
