import type { Environment, ProjectDetail, ServiceSummary } from "@/lib/api";

/** The environment to show: the one in the URL, else the one last used in this
 *  project, else the default (first) one. Unknown ids (deleted) are skipped. */
export function pickEnvironment(envs: Environment[], fromUrl: string | undefined, remembered: string | null): Environment | undefined {
  return envs.find((e) => e.id === fromUrl) ?? envs.find((e) => e.id === remembered) ?? envs[0];
}

const storageKey = (projectId: string) => `modula:env:${projectId}`;

/** The environment last used in this project, if the browser kept it. */
export function rememberedEnvironment(projectId: string): string | null {
  try {
    return localStorage.getItem(storageKey(projectId));
  } catch {
    return null;
  }
}

export function rememberEnvironment(projectId: string, envId: string) {
  try {
    localStorage.setItem(storageKey(projectId), envId);
  } catch {
    // Storage unavailable (private mode): the URL still carries the choice.
  }
}

export interface RepoService {
  serviceId: string;
  name: string;
  repo: string;
  branch: string;
}

/** The services of `envId` that deploy from a repository, with their branch. */
export function repoServices(project: ProjectDetail, envId: string): RepoService[] {
  return project.instances.flatMap((i) => {
    if (i.environmentId !== envId || i.sourceType !== "repo" || !i.repo) return [];
    const name = project.services.find((s) => s.id === i.serviceId)?.name ?? i.serviceId;
    return [{ serviceId: i.serviceId, name, repo: i.repo, branch: i.branch ?? "" }];
  });
}

/** The `branches` of a create request: only edited, non-empty branches that differ from the source's. */
export function branchOverrides(services: RepoService[], edits: Record<string, string>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const s of services) {
    const b = edits[s.serviceId]?.trim();
    if (b && b !== s.branch) out[s.serviceId] = b;
  }
  return out;
}

/** The services that exist in `envId` (a service deleted from an environment
 *  has no instance there and isn't shown in it). */
export function servicesIn(project: ProjectDetail, envId: string): ServiceSummary[] {
  return project.services.filter((s) => project.instances.some((i) => i.serviceId === s.id && i.environmentId === envId));
}

export interface ServiceDeletion {
  /** The service's instance in the environment (absent: not in it). */
  instanceId?: string;
  environmentName?: string;
  /** It has a volume there, whose data deleting it deletes. */
  volume: boolean;
  r2: boolean;
  /** The names of every environment the service is in, this one included. */
  environments: string[];
}

/** What deleting a service from one environment touches, for its confirmation. */
export function serviceDeletion(project: ProjectDetail | undefined, serviceId: string, envId: string): ServiceDeletion {
  const instances = project?.instances.filter((i) => i.serviceId === serviceId) ?? [];
  const here = instances.find((i) => i.environmentId === envId);
  const envName = (id: string) => project?.environments.find((e) => e.id === id)?.name ?? id;
  return {
    instanceId: here?.id,
    environmentName: project?.environments.find((e) => e.id === envId)?.name,
    volume: !!here?.volume,
    r2: project?.services.find((s) => s.id === serviceId)?.template === "r2",
    environments: instances.map((i) => envName(i.environmentId)),
  };
}
