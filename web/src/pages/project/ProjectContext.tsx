import { createContext, useContext } from "react";
import type { Environment, ProjectDetail, Role } from "@/lib/api";

/** What the project shell shares with the pages in it: the loaded project and the selected environment. */
export interface ProjectCtx {
  projectId: string;
  project: ProjectDetail | undefined;
  /** The environment from `?env=`, else the first one. */
  env: Environment | undefined;
  role: Role | undefined;
}

export const ProjectContext = createContext<ProjectCtx | null>(null);

export function useProject(): ProjectCtx {
  const ctx = useContext(ProjectContext);
  if (!ctx) throw new Error("useProject outside the project shell");
  return ctx;
}
