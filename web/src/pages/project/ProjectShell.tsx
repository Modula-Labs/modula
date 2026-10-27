import { useQuery } from "@tanstack/react-query";
import { Link, Outlet, useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { ChartBar, Check, ChevronDown, FileText, LayoutGrid, Plus, Refresh, Server, Settings, IconStack2 as Stack, Variable } from "@/components/ui/icons";
import { LogoMark } from "@/components/ui/Logo";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/Menu";
import { Badge, Skeleton } from "@/components/ui/misc";
import { ApiError, api } from "@/lib/api";
import { atLeast } from "@/lib/roles";
import { isLive } from "@/lib/status";
import { useMe } from "@/lib/workspace";
import { ProjectContext, type ProjectCtx } from "./ProjectContext";
import { pickEnvironment, rememberEnvironment, rememberedEnvironment } from "./environments";
import { NewEnvironmentDialog } from "./ProjectDialogs";
import { SyncDialog } from "./SyncDialog";

/** The project frame shared by the canvas, metrics, logs and settings pages: top bar with the
 *  environment selector, the left icon rail, and the page itself. The selected environment lives
 *  in `?env=` so it survives switching between pages, and is remembered per project for visits
 *  without it. */
export function ProjectShell({ projectId, envId }: { projectId: string; envId: string | undefined }) {
  const { data: me } = useMe();
  const navigate = useNavigate();
  const [newEnv, setNewEnv] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const project = useQuery({
    queryKey: ["project", projectId],
    queryFn: () => api.project(projectId),
    refetchInterval: (q) =>
      q.state.data?.instances.some((i) => i.latestDeploymentStatus && isLive(i.latestDeploymentStatus)) ? 2000 : 10_000,
  });
  const p = project.data;
  const role = me?.workspaces.find((w) => w.id === p?.workspaceId)?.role;
  // Read once per project: the stored value changes with every switch.
  const remembered = useMemo(() => rememberedEnvironment(projectId), [projectId]);
  const env = p ? pickEnvironment(p.environments, envId, remembered) : undefined;
  const ctx: ProjectCtx = useMemo(() => ({ projectId, project: p, env, role }), [projectId, p, env, role]);
  const canEdit = atLeast(role, "member");

  // Stay on the current page; a canvas selection doesn't carry over to another environment.
  const setEnv = (id: string | undefined) => void navigate({ to: ".", search: id ? { env: id } : {} } as never);

  const shownId = env?.id;
  useEffect(() => {
    if (!shownId) return;
    rememberEnvironment(projectId, shownId);
    // Opened without `?env=` on a remembered environment: put it in the URL.
    if (!envId && shownId === remembered) {
      void navigate({ to: ".", search: (s: Record<string, unknown>) => ({ ...s, env: shownId }), replace: true } as never);
    }
  }, [projectId, shownId, envId, remembered, navigate]);

  if (project.error) {
    const missing = project.error instanceof ApiError && project.error.status === 404;
    return (
      <div className="flex h-full items-center justify-center p-8">
        <div className="flex max-w-sm flex-col items-center gap-3 text-center">
          <div className="text-lg font-bold">{missing ? "Project not found" : "Couldn't load the project"}</div>
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
        <Link to="/projects" aria-label="All projects" className="flex items-center justify-center rounded-lg text-foreground">
          <LogoMark />
        </Link>
        <header className="flex min-w-0 items-center gap-2 pr-3">
          <Link to="/projects" className="truncate text-sm text-muted-foreground hover:text-foreground">
            Projects
          </Link>
          <span className="text-muted-foreground">/</span>
          {p ? <span className="truncate text-sm font-bold">{p.name}</span> : <Skeleton className="h-5 w-32" />}
          <span className="text-muted-foreground">/</span>
          {p && env ? (
            <Menu
              align="start"
              trigger={
                <button className="flex h-8 cursor-pointer items-center gap-1.5 rounded-lg px-2 text-sm hover:bg-nav-hover">
                  {env.name}
                  {env.prNumber ? <Badge>PR #{env.prNumber}</Badge> : null}
                  <ChevronDown className="size-3.5 text-muted-foreground" />
                </button>
              }
            >
              {p.environments.map((e) => (
                <MenuItem key={e.id} icon={e.id === env.id ? <Check /> : <span className="size-4" />} onSelect={() => setEnv(e.id)}>
                  <span className="flex items-center gap-1.5">
                    {e.name}
                    {e.prNumber ? <Badge>PR #{e.prNumber}</Badge> : null}
                  </span>
                </MenuItem>
              ))}
              <MenuSeparator />
              {canEdit ? (
                <MenuItem icon={<Plus />} onSelect={() => setNewEnv(true)}>
                  New environment
                </MenuItem>
              ) : null}
              {canEdit && p.environments.length > 1 ? (
                <MenuItem icon={<Refresh />} onSelect={() => setSyncing(true)}>
                  Sync {env.name} from…
                </MenuItem>
              ) : null}
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
              <MenuItem
                icon={<Settings />}
                onSelect={() =>
                  void navigate({
                    to: "/projects/$projectId/settings/$section",
                    params: { projectId, section: "environments" },
                    search: { env: env.id },
                  } as never)
                }
              >
                Manage environments
              </MenuItem>
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

        <main className="relative mr-2 mb-2 min-h-0 overflow-hidden rounded-xl border border-border bg-background">
          <Outlet />
        </main>
      </div>

      {p ? <NewEnvironmentDialog open={newEnv} onOpenChange={setNewEnv} project={p} current={env} onCreated={(id) => setEnv(id)} /> : null}
      {p && env ? <SyncDialog open={syncing} onOpenChange={setSyncing} projectId={projectId} target={env} environments={p.environments} /> : null}
    </ProjectContext.Provider>
  );
}

const railClass =
  "group relative flex size-8 items-center justify-center rounded-lg text-muted-foreground hover:bg-nav-hover hover:text-foreground focus-visible:bg-nav-hover focus-visible:text-foreground [&>svg]:size-4";
const railActive = { className: "!bg-nav-hover !text-foreground [&>svg]:!text-foreground", "aria-current": "page" as const };

function Tip({ children }: { children: ReactNode }) {
  return (
    <span
      role="tooltip"
      className="pointer-events-none absolute top-1/2 left-full z-30 ml-2 -translate-y-1/2 rounded-lg bg-popover px-2 py-1 text-xs whitespace-nowrap text-popover-foreground opacity-0 shadow-menu transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100"
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
