import { uniqueName } from "./templates";

/** What the New project picker can start from; each but `empty` opens the matching add-a-service step. */
export type NewProjectKind = "repo" | "database" | "templates" | "image" | "function" | "bucket" | "empty";

export interface NewProjectOption {
  kind: NewProjectKind;
  label: string;
  /** Has a sub-step (chevron). */
  sub: boolean;
  /** Extra words the filter matches besides the label. */
  keywords: string;
}

export const NEW_PROJECT_OPTIONS: NewProjectOption[] = [
  { kind: "repo", label: "GitHub Repository", sub: true, keywords: "git github repo source code deploy" },
  { kind: "database", label: "Database", sub: true, keywords: "postgres postgresql mysql mariadb mongo mongodb redis valkey clickhouse sql" },
  { kind: "templates", label: "Template", sub: true, keywords: "app apps one-click starter" },
  { kind: "image", label: "Docker Image", sub: true, keywords: "container registry ghcr hub" },
  { kind: "function", label: "Function", sub: false, keywords: "worker serverless edge javascript" },
  { kind: "bucket", label: "Bucket", sub: false, keywords: "s3 storage r2 files object" },
  { kind: "empty", label: "Empty Project", sub: false, keywords: "blank nothing canvas" },
];

/** Options matching every word of `query` (label or keywords), in list order. */
export function filterNewProjectOptions(query: string, options: NewProjectOption[] = NEW_PROJECT_OPTIONS): NewProjectOption[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return options;
  return options.filter((o) => {
    const text = `${o.label} ${o.keywords}`.toLowerCase();
    return words.every((w) => text.includes(w));
  });
}

/** `owner/name` when `input` is a GitHub link (`https://github.com/o/n`, `github.com/o/n.git`, `git@github.com:o/n`), else null.
 *  A bare `owner/name` is not a link: it stays a search. */
export function githubRepoFromLink(input: string): string | null {
  const m = input
    .trim()
    .match(/^(?:(?:https?:\/\/)?(?:www\.)?github\.com\/|git@github\.com:)([A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)\/([A-Za-z0-9._-]+?)(?:\.git)?(?:[/?#].*)?$/i);
  return m ? `${m[1]}/${m[2]}` : null;
}

/** A default project name: the repository's name when there is one, else "my-project"; numbered when taken. */
export function defaultProjectName(taken: string[], repo?: string | null): string {
  const base = (repo?.split("/").pop() ?? "").toLowerCase().replace(/[^a-z0-9-_ ]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 60);
  return uniqueName(base || "my-project", taken);
}

/** The `?add=` values the project canvas understands (opens the New Service dialog on that step). */
export const ADD_STEPS = ["repo", "image", "function", "database", "bucket", "templates"] as const;
export type AddStep = (typeof ADD_STEPS)[number];
export const addStepFrom = (v: unknown): AddStep | undefined => ADD_STEPS.find((s) => s === v);
