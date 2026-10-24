import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Plus } from "@/components/ui/icons";
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
  const [creating, setCreating] = useState(false);
  const [syncing, setSyncing] = useState<Environment>();
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteEnvironment(id),
    onSuccess: async (_, id) => {
      await qc.invalidateQueries({ queryKey: ["project", project.id] });
      // The shown environment is gone: fall back to the first one.
      if (id === current?.id) void navigate({ to: ".", search: {} } as never);
      setDeleting(undefined);
    },
  });
  return (
    <SettingsSection title="Environments" description="Each environment has its own copy of every service, with its own variables and domains. The first one can't be deleted.">
      {remove.error ? <Alert>{remove.error.message}</Alert> : null}
      <ul className="flex max-w-2xl flex-col gap-2">
        {project.environments.map((e, i) => (
          <li key={e.id} className="rounded-md border border-border">
            <div className="flex h-11 items-center gap-2 pr-2 pl-3">
              <span className="min-w-0 flex-1 truncate text-sm">{e.name}</span>
              {e.id === current?.id ? <Badge>Current</Badge> : null}
              {canEdit && project.environments.length > 1 ? (
                <Button size="sm" variant="ghost" onClick={() => setSyncing(e)}>
                  Sync from…
                </Button>
              ) : null}
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
      {canEdit ? (
        <div>
          <Button icon={<Plus />} onClick={() => setCreating(true)}>
            New environment
          </Button>
        </div>
      ) : null}
      <NewEnvironmentDialog
        open={creating}
        onOpenChange={setCreating}
        projectId={project.id}
        environments={project.environments}
        current={current}
        onCreated={() => undefined}
      />
      {syncing ? (
        <SyncDialog open onOpenChange={(o) => !o && setSyncing(undefined)} projectId={project.id} target={syncing} environments={project.environments} />
      ) : null}
    </SettingsSection>
  );
}
