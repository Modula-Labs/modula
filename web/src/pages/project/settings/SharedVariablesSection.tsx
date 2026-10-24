import { useState } from "react";
import { Field, Select } from "@/components/ui/Input";
import { Skeleton } from "@/components/ui/misc";
import type { ProjectDetail } from "@/lib/api";
import { SharedVariables } from "../SharedVariables";
import { SettingsSection } from "./SettingsParts";

/** Shared variables of one environment, chosen here (starts on the one selected in the top bar). */
export function SharedVariablesSection({ project, currentEnvId, canEdit }: { project: ProjectDetail; currentEnvId: string | undefined; canEdit: boolean }) {
  const [picked, setPicked] = useState<string>();
  const envId = picked ?? currentEnvId ?? project.environments[0]?.id;
  return (
    <SettingsSection title="Shared Variables" description={<>One set per environment that every service in it can read with <code className="font-mono">{"${{shared.NAME}}"}</code>.</>}>
      <div className="flex max-w-xl flex-col gap-4">
        <Field label="Environment">
          {(p) => (
            <Select {...p} value={envId ?? ""} onChange={(e) => setPicked(e.target.value)}>
              {project.environments.map((e) => (
                <option key={e.id} value={e.id}>
                  {e.name}
                </option>
              ))}
            </Select>
          )}
        </Field>
      </div>
      {envId ? <SharedVariables key={envId} environmentId={envId} canEdit={canEdit} /> : <Skeleton className="h-32" />}
    </SettingsSection>
  );
}
