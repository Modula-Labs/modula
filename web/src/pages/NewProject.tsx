import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { clsx } from "clsx";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { BrandDocker, BrandGithub, Bucket, ChevronRight, Database, LayoutGrid, Search, Terminal, Zap } from "@/components/ui/icons";
import { Spinner } from "@/components/ui/Button";
import { Alert } from "@/components/ui/misc";
import { api } from "@/lib/api";
import { moveHighlight } from "@/lib/github";
import {
  addStepFrom,
  defaultProjectName,
  filterNewProjectOptions,
  githubRepoFromLink,
  type NewProjectKind,
} from "@/lib/newProject";
import { useCurrentWorkspace, useMe } from "@/lib/workspace";

const ICONS: Record<NewProjectKind, ReactNode> = {
  repo: <BrandGithub />,
  database: <Database />,
  templates: <LayoutGrid />,
  image: <BrandDocker />,
  function: <Zap />,
  bucket: <Bucket />,
  empty: <Terminal />,
};

/**
 * Railway-style "New project": the empty canvas with one centred card. Paste a GitHub link to start
 * from that repository, or type to filter the sources. Choosing one creates the project (named
 * after the repository, else "my-project") and opens its canvas on the matching add-a-service step.
 */
export function NewProjectPage() {
  const { data: me } = useMe();
  const [workspace] = useCurrentWorkspace(me);
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const projects = useQuery({
    queryKey: ["projects", workspace?.id],
    queryFn: () => api.projects(workspace!.id),
    enabled: !!workspace,
  });
  const canCreate = !!workspace && workspace.role !== "viewer" && workspace.role !== "deployer";

  const repoLink = githubRepoFromLink(query);
  const options = filterNewProjectOptions(repoLink ? "" : query);
  const current = Math.min(active, options.length - 1);

  useEffect(() => input.current?.focus(), []);
  useEffect(() => setActive(0), [query]);

  const create = useMutation({
    mutationFn: async (pick: { kind: NewProjectKind; repo?: string }) => {
      const taken = projects.data?.map((p) => p.name) ?? (await api.projects(workspace!.id)).map((p) => p.name);
      const project = await api.createProject(workspace!.id, { name: defaultProjectName(taken, pick.repo) });
      return { project, pick };
    },
    onSuccess: async ({ project, pick }) => {
      await qc.invalidateQueries({ queryKey: ["projects", workspace?.id] });
      await navigate({
        to: "/projects/$projectId",
        params: { projectId: project.id },
        search: { add: addStepFrom(pick.kind), repo: pick.repo },
      });
    },
  });
  const choose = (kind: NewProjectKind, repo?: string) => {
    if (canCreate && !create.isPending) create.mutate({ kind, repo });
  };
  const back = () => void navigate({ to: "/projects" });

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      setActive(moveHighlight(current, options.length, e.key === "ArrowDown" ? 1 : -1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (repoLink) choose("repo", repoLink);
      else if (options[current]) choose(options[current].kind);
    } else if (e.key === "Escape") {
      e.preventDefault();
      if (query) setQuery("");
      else back();
    }
  };

  return (
    <div className="dotgrid -m-px flex min-h-[calc(100vh-7rem)] items-center justify-center rounded-xl border border-border px-4 py-10">
      <div className="w-full max-w-[480px] rounded-2xl border border-border bg-popover p-1.5 text-popover-foreground shadow-menu">
        {!canCreate && workspace ? (
          <div className="p-1.5">
            <Alert tone="warn">Your role in this workspace can't create projects.</Alert>
          </div>
        ) : null}
        {create.error ? (
          <div className="p-1.5">
            <Alert>{create.error.message}</Alert>
          </div>
        ) : null}
        <label className="flex h-11 items-center gap-2.5 rounded-xl border border-input px-3 text-muted-foreground focus-within:border-primary focus-within:ring-2 focus-within:ring-primary/30 dark:bg-input/30">
          {create.isPending ? <Spinner /> : <Search className="size-4" />}
          <input
            ref={input}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={onKeyDown}
            disabled={!canCreate}
            role="combobox"
            aria-expanded
            aria-controls="new-project-options"
            aria-activedescendant={!repoLink && options[current] ? `np-${options[current].kind}` : undefined}
            aria-label="Describe your project or paste a repo link"
            placeholder="Paste a GitHub repo link or search…"
            className="min-w-0 flex-1 bg-transparent text-sm text-foreground outline-none placeholder:text-muted-foreground"
          />
          <kbd className="hidden rounded border border-border px-1.5 text-[11px] text-muted-foreground sm:block">Esc</kbd>
        </label>
        <ul id="new-project-options" role="listbox" aria-label="Start from" className="mt-1.5 flex flex-col">
          {repoLink ? (
            <li role="option" id="np-repo-link" aria-selected>
              <Row
                active
                disabled={!canCreate || create.isPending}
                icon={ICONS.repo}
                label={`Deploy ${repoLink}`}
                hint="from GitHub"
                onClick={() => choose("repo", repoLink)}
              />
            </li>
          ) : (
            options.map((o, i) => (
              <li key={o.kind} role="option" id={`np-${o.kind}`} aria-selected={i === current}>
                <Row
                  active={i === current}
                  disabled={!canCreate || create.isPending}
                  icon={ICONS[o.kind]}
                  label={o.label}
                  chevron={o.sub}
                  onHover={() => setActive(i)}
                  onClick={() => choose(o.kind)}
                />
              </li>
            ))
          )}
          {!repoLink && options.length === 0 ? (
            <li className="px-3 py-6 text-center text-sm text-muted-foreground">Nothing matches “{query.trim()}”.</li>
          ) : null}
        </ul>
      </div>
    </div>
  );
}

function Row({
  icon,
  label,
  hint,
  chevron,
  active,
  disabled,
  onHover,
  onClick,
}: {
  icon: ReactNode;
  label: string;
  hint?: string;
  chevron?: boolean;
  active: boolean;
  disabled?: boolean;
  onHover?: () => void;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      tabIndex={-1}
      disabled={disabled}
      onClick={onClick}
      onMouseMove={onHover}
      className={clsx(
        "flex h-11 w-full cursor-pointer items-center gap-3 rounded-xl px-3 text-left text-sm disabled:cursor-default disabled:opacity-50 [&>svg]:size-[18px]",
        active ? "bg-accent text-foreground [&>svg]:text-primary" : "text-muted-foreground [&>svg]:text-muted-foreground",
      )}
    >
      {icon}
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {hint ? <span className="text-xs text-muted-foreground">{hint}</span> : null}
      {chevron ? <ChevronRight className="size-4" /> : null}
    </button>
  );
}
