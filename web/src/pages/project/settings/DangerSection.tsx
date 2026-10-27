import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Card, CardContent, CardHeader } from "@/components/ui/Card";
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
        <Card>
          <CardHeader title="Services" description="Remove a service and its deployments from this project." />
          <CardContent>
            {project.services.length === 0 ? (
              <p className="text-sm text-muted-foreground">No services.</p>
            ) : (
              <ul className="flex flex-col">
                {project.services.map((s) => (
                  <li key={s.id} className="-mx-3 flex min-h-11 items-center gap-2 rounded-lg px-3 py-1.5 hover:bg-muted/50">
                    <span className="min-w-0 flex-1 truncate text-sm">{s.name}</span>
                    {canEdit ? (
                      <Button variant="danger-outline" onClick={() => setService({ id: s.id, name: s.name })}>
                        Remove
                      </Button>
                    ) : null}
                  </li>
                ))}
              </ul>
            )}
          </CardContent>
        </Card>

        {remove.error ? <Alert>{remove.error.message}</Alert> : null}
        <Card>
          <CardHeader
            title="Delete project"
            description="Removes every service, environment and deployment, and stops all containers."
            action={
              confirming || !canDelete ? null : (
                <Button variant="danger-outline" onClick={() => setConfirming(true)}>
                  Delete project
                </Button>
              )
            }
          />
          {!canDelete || confirming ? (
            <CardContent>
              {!canDelete ? <p className="text-[13px] text-muted-foreground">Only workspace admins can delete a project.</p> : null}
              {confirming ? (
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
              ) : null}
            </CardContent>
          ) : null}
        </Card>
      </div>
      <DeleteServiceDialog service={service} onClose={() => setService(undefined)} projectId={project.id} onDeleted={() => undefined} />
    </SettingsSection>
  );
}
