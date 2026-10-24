import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Alert } from "@/components/ui/misc";
import { api, type ProjectDetail } from "@/lib/api";
import { DeleteServiceDialog } from "../ProjectDialogs";
import { ConfirmDelete, SettingsSection } from "./SettingsParts";

export function DangerSection({ project, canEdit, canDelete }: { project: ProjectDetail; canEdit: boolean; canDelete: boolean }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [confirming, setConfirming] = useState(false);
  const [service, setService] = useState<{ id: string; name: string }>();
  const remove = useMutation({
    mutationFn: () => api.deleteProject(project.id),
    onSuccess: async () => {
      qc.removeQueries({ queryKey: ["project", project.id] });
      await qc.invalidateQueries({ queryKey: ["projects"] });
      void navigate({ to: "/projects" });
    },
  });
  return (
    <SettingsSection title="Danger" description="These can't be undone.">
      <div className="flex max-w-2xl flex-col gap-4">
        <div className="rounded-lg border border-destructive/40">
          <div className="border-b border-destructive/40 px-4 py-3 text-sm font-medium">Services</div>
          {project.services.length === 0 ? (
            <p className="px-4 py-3 text-sm text-muted-foreground">No services.</p>
          ) : (
            <ul>
              {project.services.map((s) => (
                <li key={s.id} className="flex h-11 items-center gap-2 border-b border-destructive/20 pr-2 pl-4 last:border-b-0">
                  <span className="min-w-0 flex-1 truncate text-sm">{s.name}</span>
                  {canEdit ? (
                    <Button size="sm" variant="danger-outline" onClick={() => setService({ id: s.id, name: s.name })}>
                      Remove
                    </Button>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </div>

        {remove.error ? <Alert>{remove.error.message}</Alert> : null}
        <div className="rounded-lg border border-destructive/40 p-4">
          <div className="flex items-center justify-between gap-4">
            <div className="text-sm">
              <div className="font-medium">Delete project</div>
              <div className="text-muted-foreground">Removes every service, environment and deployment, and stops all containers.</div>
            </div>
            {confirming || !canDelete ? null : (
              <Button variant="danger" onClick={() => setConfirming(true)}>
                Delete project
              </Button>
            )}
          </div>
          {!canDelete ? <p className="mt-3 text-xs text-muted-foreground">Only workspace admins can delete a project.</p> : null}
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
      </div>
      <DeleteServiceDialog service={service} onClose={() => setService(undefined)} projectId={project.id} onDeleted={() => undefined} />
    </SettingsSection>
  );
}
