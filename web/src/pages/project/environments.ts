import type { Environment, ProjectDetail } from "@/lib/api";

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
