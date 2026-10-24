import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Field, Input, Textarea } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { api, type ProjectDetail } from "@/lib/api";
import { SettingsSection } from "./SettingsParts";

export function GeneralSection({ project, canEdit }: { project: ProjectDetail; canEdit: boolean }) {
  const qc = useQueryClient();
  const [name, setName] = useState(project.name);
  const [description, setDescription] = useState(project.description);
  const dirty = name.trim() !== project.name || description.trim() !== project.description;
  const save = useMutation({
    mutationFn: () => api.updateProject(project.id, { name: name.trim(), description: description.trim() }),
    onSuccess: () =>
      Promise.all([qc.invalidateQueries({ queryKey: ["project", project.id] }), qc.invalidateQueries({ queryKey: ["projects"] })]),
  });
  return (
    <SettingsSection title="General" description="How this project appears in the workspace.">
      <form
        className="flex max-w-xl flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          save.mutate();
        }}
      >
        {save.error ? <Alert>{save.error.message}</Alert> : null}
        <Field label="Name">
          {(p) => <Input {...p} required maxLength={64} disabled={!canEdit} value={name} onChange={(e) => setName(e.target.value)} />}
        </Field>
        <Field label="Description">
          {(p) => <Textarea {...p} rows={3} maxLength={280} disabled={!canEdit} value={description} onChange={(e) => setDescription(e.target.value)} />}
        </Field>
        {canEdit ? (
          <div className="flex items-center gap-3">
            <Button type="submit" variant="primary" loading={save.isPending} disabled={!dirty || !name.trim()}>
              Update
            </Button>
            {save.isSuccess && !dirty ? <span className="text-sm text-muted-foreground">Saved</span> : null}
          </div>
        ) : (
          <p className="text-xs text-muted-foreground">Members and admins can edit the project.</p>
        )}
      </form>
      <div className="flex items-center gap-3 text-sm">
        <span className="w-24 text-muted-foreground">Project ID</span>
        <code className="font-mono text-[13px]">{project.id}</code>
        <CopyButton value={project.id} label="Copy project ID" />
      </div>
    </SettingsSection>
  );
}
