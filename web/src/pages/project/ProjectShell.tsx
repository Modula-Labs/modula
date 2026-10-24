import { useQuery } from "@tanstack/react-query";
import { Link, Outlet, useNavigate } from "@tanstack/react-router";
import { useMemo, useState, type ReactNode } from "react";
import { ChartBar, Check, ChevronDown, FileText, LayoutGrid, Plus, Server, Settings, IconStack2 as Stack, Trash, Variable } from "@/components/ui/icons";
import { LogoMark } from "@/components/ui/Logo";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/Menu";
import { Skeleton } from "@/components/ui/misc";
import { ApiError, api, type Environment } from "@/lib/api";
import { atLeast } from "@/lib/roles";
import { isLive } from "@/lib/status";
import { useMe } from "@/lib/workspace";
import { ProjectContext, type ProjectCtx } from "./ProjectContext";
import { DeleteEnvironmentDialog, NewEnvironmentDialog } from "./ProjectDialogs";

/** The project frame shared by the canvas, metrics, logs and settings pages: top bar with the
 *  environment selector, the left icon rail, and the page itself. The selected environment lives
 *  in `?env=` so it survives switching between pages. */
export function ProjectShell({ projectId, envId }: { projectId: string; envId: string | undefined }) {
  const { data: me } = useMe();
  const navigate = useNavigate();
  const [newEnv, setNewEnv] = useState(false);
  const [deletingEnv, setDeletingEnv] = useState<Environment>();
  const project = useQuery({
    queryKey: ["project", projectId],
    queryFn: () => api.project(projectId),
    refetchInterval: (q) =>
      q.state.data?.instances.some((i) => i.latestDeploymentStatus && isLive(i.latestDeploymentStatus)) ? 2000 : 10_000,
  });
  const p = project.data;
  const role = me?.workspaces.find((w) => w.id === p?.workspaceId)?.role;
  const env = p?.environments.find((e) => e.id === envId) ?? p?.environments[0];
  const ctx: ProjectCtx = useMemo(() => ({ projectId, project: p, env, role }), [projectId, p, env, role]);
  const canEdit = atLeast(role, "member");

  // Stay on the current page; a canvas selection doesn't carry over to another environment.
  const setEnv = (id: string | undefined) => void navigate({ to: ".", search: id ? { env: id } : {} } as never);

  if (project.error) {
    const missing = project.error instanceof ApiError && project.error.status === 404;
    return (
      <div className="flex h-full items-center justify-center p-8">
        <div className="flex max-w-sm flex-col items-center gap-3 text-center">
          <div className="text-lg font-semibold">{missing ? "Project not found" : "Couldn't load the project"}</div>
          <p className="text-sm text-muted-foreground">{missing ? "It may have been deleted, or you don't have access." : project.error.message}</p>
          <Link to="/projects" className="text-sm underline">
            Back to projects
          </Link>
        </div>
      </div>
    );
  }

  return (
    <ProjectContext.Provider value={ctx}>
      <div className="grid h-full grid-cols-[56px_1fr] grid-rows-[52px_1fr]">
        <Link to="/projects" aria-label="All projects" className="flex items-center justify-center rounded-md text-foreground">
          <LogoMark />
        </Link>
        <header className="flex min-w-0 items-center gap-2 pr-3">
          <Link to="/projects" className="truncate text-sm text-muted-foreground hover:text-foreground">
            Projects
          </Link>
          <span className="text-muted-foreground">/</span>
          {p ? <span className="truncate text-sm font-semibold">{p.name}</span> : <Skeleton className="h-5 w-32" />}
          <span className="text-muted-foreground">/</span>
          {p && env ? (
            <Menu
              align="start"
              trigger={
                <button className="flex h-8 items-center gap-1.5 rounded-md px-2 text-sm hover:bg-accent">
                  {env.name}
                  <ChevronDown className="size-3.5 text-muted-foreground" />
                </button>
              }
            >
              {p.environments.map((e) => (
                <MenuItem key={e.id} icon={e.id === env.id ? <Check /> : <span className="size-4" />} onSelect={() => setEnv(e.id)}>
                  {e.name}
                </MenuItem>
              ))}
              <MenuSeparator />
              <MenuItem
                icon={<Variable />}
                onSelect={() =>
                  void navigate({
                    to: "/projects/$projectId/settings/$section",
                    params: { projectId, section: "shared-variables" },
                    search: { env: env.id },
                  } as never)
                }
              >
                Shared variables
              </MenuItem>
              {canEdit ? (
                <MenuItem icon={<Plus />} onSelect={() => setNewEnv(true)}>
                  New environment
                </MenuItem>
              ) : null}
              {atLeast(role, "admin") && env.id !== p.environments[0]?.id ? (
                <MenuItem danger icon={<Trash />} onSelect={() => setDeletingEnv(env)}>
                  Delete {env.name}
                </MenuItem>
              ) : null}
            </Menu>
          ) : null}
        </header>

        <nav aria-label="Project" className="flex flex-col items-center gap-1 pt-2">
          <RailLink to="/projects/$projectId" projectId={projectId} env={envId} label="Canvas" icon={<Stack />} exact />
          <RailLink to="/projects/$projectId/metrics" projectId={projectId} env={envId} label="Metrics" icon={<ChartBar />} />
          <RailLink to="/projects/$projectId/logs" projectId={projectId} env={envId} label="Logs" icon={<FileText />} />
          <RailLink to="/projects/$projectId/settings" projectId={projectId} env={envId} label="Settings" icon={<Settings />} />
          <div className="mt-auto flex flex-col items-center gap-1 pb-2">
            <PlainRailLink to="/projects" label="All projects" icon={<LayoutGrid />} />
            {me?.user.isInstanceAdmin ? <PlainRailLink to="/servers" label="Servers" icon={<Server />} /> : null}
          </div>
        </nav>

        <main className="relative mr-2 mb-2 min-h-0 overflow-hidden rounded-xl border border-border">
          <Outlet />
        </main>
      </div>

      {p ? (
        <NewEnvironmentDialog
          open={newEnv}
          onOpenChange={setNewEnv}
          projectId={projectId}
          environments={p.environments}
          current={env}
          onCreated={(id) => setEnv(id)}
        />
      ) : null}
      <DeleteEnvironmentDialog environment={deletingEnv} onClose={() => setDeletingEnv(undefined)} projectId={projectId} onDeleted={() => setEnv(undefined)} />
    </ProjectContext.Provider>
  );
}

const railClass =
  "group relative flex size-9 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground focus-visible:bg-accent focus-visible:text-foreground [&>svg]:size-[18px]";
const railActive = { className: "!bg-accent !text-foreground [&>svg]:text-brand-text", "aria-current": "page" as const };

function Tip({ children }: { children: ReactNode }) {
  return (
    <span
      role="tooltip"
      className="pointer-events-none absolute top-1/2 left-full z-30 ml-2 -translate-y-1/2 rounded-md border border-border bg-popover px-2 py-1 text-xs whitespace-nowrap text-popover-foreground opacity-0 shadow-raise transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100"
    >
      {children}
    </span>
  );
}

/** A rail link into a project page; carries the selected environment along. */
function RailLink({
  to,
  projectId,
  env,
  label,
  icon,
  exact,
}: {
  to: "/projects/$projectId" | "/projects/$projectId/metrics" | "/projects/$projectId/logs" | "/projects/$projectId/settings";
  projectId: string;
  env: string | undefined;
  label: string;
  icon: ReactNode;
  exact?: boolean;
}) {
  return (
    <Link
      to={to}
      params={{ projectId }}
      search={{ env } as never}
      aria-label={label}
      className={railClass}
      activeOptions={{ exact, includeSearch: false }}
      activeProps={railActive}
    >
      {icon}
      <Tip>{label}</Tip>
    </Link>
  );
}

function PlainRailLink({ to, label, icon }: { to: string; label: string; icon: ReactNode }) {
  return (
    <Link to={to} aria-label={label} className={railClass}>
      {icon}
      <Tip>{label}</Tip>
    </Link>
  );
}
