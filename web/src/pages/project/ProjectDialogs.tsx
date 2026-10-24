import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { api, type Environment } from "@/lib/api";

export function NewEnvironmentDialog({
  open,
  onOpenChange,
  projectId,
  environments,
  current,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  projectId: string;
  environments: Environment[];
  /** Offered first when duplicating. */
  current?: Environment;
  onCreated: (id: string) => void;
}) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  // "" = empty: the server's default of the first environment's config, no variables.
  const [from, setFrom] = useState("");
  const [copyVariables, setCopyVariables] = useState(false);
  const create = useMutation({
    mutationFn: () =>
      api.createEnvironment(projectId, {
        name: name.trim(),
        ...(from ? { cloneFrom: from, copyVariables } : {}),
      }),
    onSuccess: async (e) => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onCreated(e.id);
      setName("");
      setFrom("");
      setCopyVariables(false);
      onOpenChange(false);
    },
  });
  const ordered = current ? [current, ...environments.filter((e) => e.id !== current.id)] : environments;
  return (
    <Modal open={open} onOpenChange={onOpenChange} title="New environment" description="Each environment gets its own copy of every service, with its own variables and domains.">
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
        <Field label="Start from" hint={from ? undefined : "Services get the first environment's configuration, without variables."}>
          {(p) => (
            <Select {...p} value={from} onChange={(e) => setFrom(e.target.value)}>
              <option value="">Empty</option>
              {ordered.map((e) => (
                <option key={e.id} value={e.id}>
                  Duplicate {e.name}
                </option>
              ))}
            </Select>
          )}
        </Field>
        {from ? (
          <label className="flex items-center gap-2 text-sm text-muted-foreground">
            <input type="checkbox" className="size-4 accent-foreground" checked={copyVariables} onChange={(e) => setCopyVariables(e.target.checked)} />
            Copy variables (sealed ones are not copied)
          </label>
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
export function DeleteEnvironmentDialog({
  environment,
  onClose,
  projectId,
  onDeleted,
}: {
  environment?: Environment;
  onClose: () => void;
  projectId: string;
  onDeleted: () => void;
}) {
  const qc = useQueryClient();
  // Keep the name on screen while the dialog fades out.
  const [last, setLast] = useState(environment);
  if (environment && environment !== last) setLast(environment);
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteEnvironment(id),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onDeleted();
      onClose();
    },
  });
  return (
    <Modal
      open={!!environment}
      onOpenChange={(o) => {
        if (!o) {
          remove.reset();
          onClose();
        }
      }}
      title={`Delete ${last?.name ?? "environment"}?`}
      description="Removes its services, variables and deployments, and stops their containers. This can't be undone."
    >
      <div className="flex flex-col gap-4">
        {remove.error ? <Alert>{remove.error.message}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="danger" loading={remove.isPending} onClick={() => last && remove.mutate(last.id)}>
            Delete environment
          </Button>
        </div>
      </div>
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
