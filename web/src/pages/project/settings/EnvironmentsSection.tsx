import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Plus } from "@/components/ui/icons";
import { Input } from "@/components/ui/Input";
import { Alert, Badge } from "@/components/ui/misc";
import { api, type Environment, type ProjectDetail } from "@/lib/api";
import { NewEnvironmentDialog } from "../ProjectDialogs";
import { SyncDialog } from "../SyncDialog";
import { ConfirmDelete, SettingsSection } from "./SettingsParts";

export function EnvironmentsSection({
  project,
  current,
  canEdit,
  canDelete,
}: {
  project: ProjectDetail;
  current: Environment | undefined;
  canEdit: boolean;
  canDelete: boolean;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [deleting, setDeleting] = useState<string>();
  const [renaming, setRenaming] = useState<string>();
  const [creating, setCreating] = useState(false);
  const [syncing, setSyncing] = useState<Environment>();
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteEnvironment(id),
    onSuccess: async (_, id) => {
      await qc.invalidateQueries({ queryKey: ["project", project.id] });
      // The shown environment is gone: fall back to the default one.
      if (id === current?.id) void navigate({ to: ".", search: { env: project.environments.find((e) => e.isDefault)?.id } } as never);
      setDeleting(undefined);
    },
  });
  return (
    <SettingsSection
      title="Environments"
      description="Each environment has its own copy of every service, with its own variables, volumes and domains. The default environment can't be deleted."
    >
      {remove.error ? <Alert>{remove.error.message}</Alert> : null}
      <ul className="flex max-w-2xl flex-col gap-2">
        {project.environments.map((e) => (
          <li key={e.id} className="rounded-lg border border-border">
            {renaming === e.id ? (
              <RenameRow
                environment={e}
                projectId={project.id}
                onDone={() => setRenaming(undefined)}
              />
            ) : (
              <div className="flex min-h-11 flex-wrap items-center gap-2 py-1.5 pr-2 pl-3">
                <span className="min-w-0 flex-1 truncate text-sm">{e.name}</span>
                {e.isDefault ? <Badge>Default</Badge> : null}
                {e.id === current?.id ? <Badge>Current</Badge> : null}
                {canEdit ? (
                  <Button variant="ghost" onClick={() => setRenaming(e.id)}>
                    Rename
                  </Button>
                ) : null}
                {canEdit && project.environments.length > 1 ? (
                  <Button variant="ghost" onClick={() => setSyncing(e)}>
                    Sync from…
                  </Button>
                ) : null}
                {!e.isDefault && canDelete && deleting !== e.id ? (
                  <Button variant="danger-outline" onClick={() => setDeleting(e.id)}>
                    Delete
                  </Button>
                ) : null}
              </div>
            )}
            {deleting === e.id ? (
              <div className="border-t border-border p-3">
                <p className="mb-3 text-sm text-muted-foreground">
                  Removes its services' deployments, variables and volumes (with their data), and stops their containers.
                </p>
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
      {canEdit ? (
        <div>
          <Button variant="primary" icon={<Plus />} onClick={() => setCreating(true)}>
            New environment
          </Button>
        </div>
      ) : null}
      <NewEnvironmentDialog
        open={creating}
        onOpenChange={setCreating}
        project={project}
        current={current}
        onCreated={(id) => void navigate({ to: ".", search: { env: id } } as never)}
      />
      {syncing ? (
        <SyncDialog open onOpenChange={(o) => !o && setSyncing(undefined)} projectId={project.id} target={syncing} environments={project.environments} />
      ) : null}
    </SettingsSection>
  );
}

function RenameRow({ environment, projectId, onDone }: { environment: Environment; projectId: string; onDone: () => void }) {
  const qc = useQueryClient();
  const [name, setName] = useState(environment.name);
  const rename = useMutation({
    mutationFn: () => api.renameEnvironment(environment.id, name.trim()),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onDone();
    },
  });
  return (
    <form
      className="flex flex-col gap-2 p-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim() === environment.name) onDone();
        else rename.mutate();
      }}
    >
      {rename.error ? <Alert>{rename.error.message}</Alert> : null}
      <div className="flex items-center gap-2">
        <Input
          aria-label={`New name for ${environment.name}`}
          autoFocus
          onFocus={(e) => e.currentTarget.select()}
          required
          maxLength={32}
          value={name}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") onDone();
          }}
        />
        <Button type="button" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
        <Button type="submit" variant="secondary" loading={rename.isPending} disabled={!name.trim()}>
          Save
        </Button>
      </div>
    </form>
  );
}
