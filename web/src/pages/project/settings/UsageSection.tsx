import { Skeleton } from "@/components/ui/misc";
import { useProject } from "../ProjectContext";
import { ProjectMetrics } from "../ProjectMetrics";
import { SettingsSection } from "./SettingsParts";

export function UsageSection() {
  const { projectId, env } = useProject();
  return (
    <SettingsSection title="Usage" description={env ? `CPU, memory and network of all services in ${env.name}.` : undefined}>
      {env ? <ProjectMetrics key={env.id} projectId={projectId} environmentId={env.id} /> : <Skeleton className="h-52" />}
    </SettingsSection>
  );
}
