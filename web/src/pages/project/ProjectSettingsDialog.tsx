import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input, Textarea } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge } from "@/components/ui/misc";
import { api, type ProjectDetail, type Role } from "@/lib/api";
import { atLeast } from "@/lib/roles";

function Section({ title, description, children }: { title: string; description?: ReactNode; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-3 border-b border-border py-5 first:pt-0 last:border-b-0 last:pb-0">
      <div>
        <h3 className="text-base font-semibold">{title}</h3>
        {description ? <p className="mt-1 text-sm text-muted-foreground">{description}</p> : null}
      </div>
      {children}
    </section>
  );
}

/** Project name, description, environments and deletion. */
export function ProjectSettingsDialog({
  open,
  onOpenChange,
  project,
  role,
  environmentId,
  onEnvironmentDeleted,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  project: ProjectDetail;
  role: Role | undefined;
  /** The environment currently shown on the canvas. */
  environmentId: string;
  /** Called after the shown environment is deleted so the caller can fall back to the first one. */
  onEnvironmentDeleted: () => void;
}) {
  return (
    <Modal open={open} onOpenChange={onOpenChange} width={560} title="Project settings">
      <div className="max-h-[65vh] overflow-y-auto">
        <SettingsBody project={project} role={role} environmentId={environmentId} onEnvironmentDeleted={onEnvironmentDeleted} />
      </div>
    </Modal>
  );
}

function SettingsBody({
  project,
  role,
  environmentId,
  onEnvironmentDeleted,
}: {
  project: ProjectDetail;
  role: Role | undefined;
  environmentId: string;
  onEnvironmentDeleted: () => void;
}) {
  const canEdit = atLeast(role, "member");
  const canDelete = atLeast(role, "admin");
  return (
    <div className="flex flex-col">
      <General project={project} canEdit={canEdit} />
      <Environments project={project} canDelete={canDelete} environmentId={environmentId} onDeleted={onEnvironmentDeleted} />
      {canDelete ? <DangerZone project={project} /> : null}
    </div>
  );
}

function General({ project, canEdit }: { project: ProjectDetail; canEdit: boolean }) {
  const qc = useQueryClient();
  const [name, setName] = useState(project.name);
  const [description, setDescription] = useState(project.description);
  const dirty = name.trim() !== project.name || description.trim() !== project.description;
  const save = useMutation({
    mutationFn: () => api.updateProject(project.id, { name: name.trim(), description: description.trim() }),
    onSuccess: () =>
      Promise.all([qc.invalidateQueries({ queryKey: ["project", project.id] }), qc.invalidateQueries({ queryKey: ["projects"] })]),
  });

  if (!canEdit) {
    return (
      <Section title="General">
        <div className="text-sm font-medium">{project.name}</div>
        <p className="text-sm text-muted-foreground">{project.description || "No description."}</p>
      </Section>
    );
  }
  return (
    <Section title="General">
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          save.mutate();
        }}
      >
        {save.error ? <Alert>{save.error.message}</Alert> : null}
        <Field label="Name">{(p) => <Input {...p} required maxLength={64} value={name} onChange={(e) => setName(e.target.value)} />}</Field>
        <Field label="Description">
          {(p) => <Textarea {...p} rows={3} maxLength={280} value={description} onChange={(e) => setDescription(e.target.value)} />}
        </Field>
        <div className="flex items-center justify-end gap-2">
          {save.isSuccess && !dirty ? <span className="text-sm text-muted-foreground">Saved</span> : null}
          <Button type="submit" variant="secondary" loading={save.isPending} disabled={!dirty || !name.trim()}>
            Save
          </Button>
        </div>
      </form>
    </Section>
  );
}

function Environments({
  project,
  canDelete,
  environmentId,
  onDeleted,
}: {
  project: ProjectDetail;
  canDelete: boolean;
  environmentId: string;
  onDeleted: () => void;
}) {
  const qc = useQueryClient();
  const [deleting, setDeleting] = useState<string>();
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteEnvironment(id),
    onSuccess: async (_, id) => {
      await qc.invalidateQueries({ queryKey: ["project", project.id] });
      if (id === environmentId) onDeleted();
      setDeleting(undefined);
    },
  });
  return (
    <Section title="Environments" description="Each environment has its own copy of every service. The first one can't be deleted.">
      {remove.error ? <Alert>{remove.error.message}</Alert> : null}
      <ul className="flex flex-col gap-2">
        {project.environments.map((e, i) => (
          <li key={e.id} className="rounded-md border border-border">
            <div className="flex h-10 items-center gap-2 pr-1 pl-3">
              <span className="min-w-0 flex-1 truncate text-sm">{e.name}</span>
              {e.id === environmentId ? <Badge>Current</Badge> : null}
              {i > 0 && canDelete && deleting !== e.id ? (
                <Button size="sm" variant="danger-outline" onClick={() => setDeleting(e.id)}>
                  Delete
                </Button>
              ) : null}
            </div>
            {deleting === e.id ? (
              <div className="border-t border-border p-3">
                <ConfirmDelete
                  name={e.name}
                  label="Delete environment"
                  pending={remove.isPending}
                  onConfirm={() => remove.mutate(e.id)}
                  onCancel={() => {
                    setDeleting(undefined);
                    remove.reset();
                  }}
                />
              </div>
            ) : null}
          </li>
        ))}
      </ul>
    </Section>
  );
}

function DangerZone({ project }: { project: ProjectDetail }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [confirming, setConfirming] = useState(false);
  const remove = useMutation({
    mutationFn: () => api.deleteProject(project.id),
    onSuccess: async () => {
      qc.removeQueries({ queryKey: ["project", project.id] });
      await qc.invalidateQueries({ queryKey: ["projects"] });
      void navigate({ to: "/projects" });
    },
  });
  return (
    <Section title="Danger zone">
      {remove.error ? <Alert>{remove.error.message}</Alert> : null}
      <div className="rounded-lg border border-destructive/40 p-4">
        <div className="flex items-center justify-between gap-4">
          <div className="text-sm">
            <div className="font-medium">Delete project</div>
            <div className="text-muted-foreground">Removes every service, environment and deployment, and stops all containers.</div>
          </div>
          {confirming ? null : (
            <Button variant="danger" onClick={() => setConfirming(true)}>
              Delete project
            </Button>
          )}
        </div>
        {confirming ? (
          <div className="mt-4">
            <ConfirmDelete
              name={project.name}
              label="Delete project"
              pending={remove.isPending}
              onConfirm={() => remove.mutate()}
              onCancel={() => {
                setConfirming(false);
                remove.reset();
              }}
            />
          </div>
        ) : null}
      </div>
    </Section>
  );
}

/** Inline "type the name to confirm" row. */
function ConfirmDelete({
  name,
  label,
  pending,
  onConfirm,
  onCancel,
}: {
  name: string;
  label: string;
  pending: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const [typed, setTyped] = useState("");
  return (
    <form
      className="flex flex-col gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        if (typed === name) onConfirm();
      }}
    >
      <Field label={`Type "${name}" to confirm`}>
        {(p) => <Input {...p} autoFocus autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />}
      </Field>
      <div className="flex justify-end gap-2">
        <Button type="button" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" variant="danger" loading={pending} disabled={typed !== name}>
          {label}
        </Button>
      </div>
    </form>
  );
}
