import { Skeleton } from "@/components/ui/misc";
import { useProject } from "./ProjectContext";
import { ProjectMetrics } from "./ProjectMetrics";

/** The Metrics page: the whole environment's CPU, memory and network. */
export function ProjectObservability() {
  const { projectId, env } = useProject();
  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex max-w-4xl flex-col gap-6 px-6 py-8 md:px-8">
        <div>
          <h1 className="text-2xl leading-8 font-bold">Metrics</h1>
          {env ? <p className="mt-1 text-sm text-muted-foreground">All services in {env.name}, summed.</p> : null}
        </div>
        {env ? <ProjectMetrics key={env.id} projectId={projectId} environmentId={env.id} /> : <Skeleton className="h-52" />}
      </div>
    </div>
  );
}
