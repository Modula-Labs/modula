import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Switch } from "@/components/ui/Controls";
import { Field, Select } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { api, type PrPreviews, type ProjectDetail } from "@/lib/api";
import { SettingsSection } from "./SettingsParts";

/** Pull request previews: on, and which environment they duplicate. Saved as soon as it changes. */
export function PrPreviewsSection({ project, canEdit }: { project: ProjectDetail; canEdit: boolean }) {
  const qc = useQueryClient();
  const current = project.prPreviews;
  const save = useMutation({
    mutationFn: (prPreviews: PrPreviews) => api.updateProject(project.id, { prPreviews }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["project", project.id] }),
  });
  const shown = save.isPending && save.variables ? save.variables : current;
  const bases = project.environments.filter((e) => !e.ephemeral);
  const defaultId = bases.find((e) => e.isDefault)?.id ?? "";
  return (
    <SettingsSection
      title="Pull request previews"
      description="Each open pull request gets its own environment, duplicated from the base environment without its sealed variables, deployed from the pull request's branch and removed when it closes. Only pull requests from the repository itself, by people with write access, get one. A comment on the pull request links each service."
    >
      {save.error ? <Alert>{save.error.message}</Alert> : null}
      <div className="flex max-w-xl flex-col gap-4">
        <Switch
          label="Create previews for pull requests"
          disabled={!canEdit || save.isPending}
          checked={shown.enabled}
          onCheckedChange={(enabled) => save.mutate({ ...current, enabled })}
        />
        <Field label="Base environment" hint="Previews copy its services, configs and non-sealed variables.">
          {(p) => (
            <Select
              {...p}
              disabled={!canEdit || save.isPending}
              value={shown.baseEnvironmentId ?? defaultId}
              onChange={(e) =>
                save.mutate({
                  ...current,
                  baseEnvironmentId: e.target.value === defaultId ? null : e.target.value,
                })
              }
            >
              {bases.map((e) => (
                <option key={e.id} value={e.id}>
                  {e.name}
                </option>
              ))}
            </Select>
          )}
        </Field>
      </div>
    </SettingsSection>
  );
}
