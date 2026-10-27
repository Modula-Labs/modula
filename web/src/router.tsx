import { QueryClient } from "@tanstack/react-query";
import { Suspense, lazy } from "react";
import { Outlet, createRootRouteWithContext, createRoute, createRouter, redirect, useNavigate } from "@tanstack/react-router";
import { RouteError, Toaster } from "./components/ErrorBoundary";
import { SudoDialog } from "./components/SudoDialog";
import { WorkspaceShell } from "./components/shell/WorkspaceShell";
import { ApiError, api } from "./lib/api";
import { cliCode } from "./lib/cliLogin";
import { inviteToken } from "./lib/members";
import { addStepFrom, githubRepoFromLink } from "./lib/newProject";
import { AccountSecurityPage } from "./pages/AccountSecurity";
import { CliLoginPage } from "./pages/CliLogin";
import type { ReturnTo } from "./pages/Login";
import { DomainOnboardingPage } from "./pages/DomainOnboarding";
import { InvitePage } from "./pages/Invite";
import { LoginPage } from "./pages/Login";
import { NewProjectPage } from "./pages/NewProject";
import { ProjectsPage } from "./pages/Projects";
import { ServersPage } from "./pages/Servers";
import { ServerDetailPage } from "./pages/servers/ServerDetail";
import { WorkspaceSettingsPage } from "./pages/WorkspaceSettings";
import type { ProjectSearch } from "./pages/project/ProjectPage";
import type { ServiceTab } from "./pages/project/ServiceSheet";
import { LogsPage } from "./pages/project/LogsPage";
import { ProjectObservability } from "./pages/project/ProjectObservability";
import { ProjectSettingsPage } from "./pages/project/ProjectSettingsPage";
import { ProjectShell } from "./pages/project/ProjectShell";
import { MaintenanceOverlay } from "./components/MaintenanceOverlay";
import { SetupPage } from "./pages/Setup";
import { TwoFactorPage } from "./pages/TwoFactor";

interface Ctx {
  queryClient: QueryClient;
}

const rootRoute = createRootRouteWithContext<Ctx>()({
  component: () => (
    <>
      <Outlet />
      <SudoDialog />
      <MaintenanceOverlay />
      <Toaster />
    </>
  ),
});

const instance = (qc: QueryClient) => qc.ensureQueryData({ queryKey: ["instance"], queryFn: api.instance });

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/projects" });
  },
});

const setupRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/setup",
  beforeLoad: async ({ context }) => {
    if (!(await instance(context.queryClient)).setupRequired) throw redirect({ to: "/login" });
  },
  component: SetupPage,
});

/** Where to go after signing in: an invite, or a CLI login to approve. */
const returnTo = (s: Record<string, unknown>): ReturnTo => ({
  ...(inviteToken(s.invite) ? { invite: inviteToken(s.invite) } : {}),
  ...(cliCode(s.cli) ? { cli: cliCode(s.cli) } : {}),
});

const loginRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login",
  validateSearch: (s: Record<string, unknown>): ReturnTo & { expired?: boolean } => ({
    ...(s.expired ? { expired: true } : {}),
    ...returnTo(s),
  }),
  beforeLoad: async ({ context }) => {
    if ((await instance(context.queryClient)).setupRequired) throw redirect({ to: "/setup" });
  },
  component: function Login() {
    const { expired, ...back } = loginRoute.useSearch();
    return <LoginPage back={back} notice={expired ? "Your session ended. Sign in again." : undefined} />;
  },
});

const twoFactorRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login/2fa",
  validateSearch: returnTo,
  component: function TwoFactor() {
    return <TwoFactorPage back={twoFactorRoute.useSearch()} />;
  },
});

/** Public: `modula login` opens this to approve the terminal's login. */
const cliLoginRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/cli/login",
  validateSearch: (s: Record<string, unknown>): { code?: string } => (cliCode(s.code) ? { code: cliCode(s.code) } : {}),
  component: function CliLogin() {
    const { code } = cliLoginRoute.useSearch();
    return <CliLoginPage code={code} />;
  },
});

/** Public: an invite link, for signed-in and signed-out visitors alike. */
const inviteRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/invite/$token",
  component: function Invite() {
    const { token } = inviteRoute.useParams();
    return <InvitePage token={token} />;
  },
});

/** Everything below requires a full session. */
const appRoute = createRoute({
  getParentRoute: () => rootRoute,
  id: "app",
  beforeLoad: async ({ context }) => {
    try {
      await context.queryClient.ensureQueryData({ queryKey: ["me"], queryFn: api.me });
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) {
        const inst = await instance(context.queryClient);
        throw redirect({ to: inst.setupRequired ? "/setup" : "/login" });
      }
      throw e;
    }
  },
  component: Outlet,
});

const domainOnboardingRoute = createRoute({ getParentRoute: () => appRoute, path: "/welcome/domain", component: DomainOnboardingPage });

/** Pages inside the workspace sidebar. */
const workspaceRoute = createRoute({ getParentRoute: () => appRoute, id: "workspace", component: WorkspaceShell });
const projectsRoute = createRoute({ getParentRoute: () => workspaceRoute, path: "/projects", component: ProjectsPage });
const newProjectRoute = createRoute({ getParentRoute: () => workspaceRoute, path: "/projects/new", component: NewProjectPage });
const securityRoute = createRoute({ getParentRoute: () => workspaceRoute, path: "/account/security", component: AccountSecurityPage });
const settingsRoute = createRoute({ getParentRoute: () => workspaceRoute, path: "/settings", component: WorkspaceSettingsPage });
const serversRoute = createRoute({ getParentRoute: () => workspaceRoute, path: "/servers", component: ServersPage });
const serverRoute = createRoute({
  getParentRoute: () => workspaceRoute,
  path: "/servers/$nodeId",
  component: function Server() {
    const { nodeId } = serverRoute.useParams();
    return <ServerDetailPage nodeId={nodeId} />;
  },
});

// The canvas (React Flow) is the heaviest part of the app; load it on demand.
const ProjectPage = lazy(() => import("./pages/project/ProjectPage").then((m) => ({ default: m.ProjectPage })));

const tabs: ServiceTab[] = ["deployments", "cron", "metrics", "database", "bucket", "variables", "shell", "backups", "settings"];
const ms = (v: unknown) => (typeof v === "number" && Number.isFinite(v) && v > 0 ? v : undefined);
const str = (v: unknown) => (typeof v === "string" && v.length <= 64 ? v : undefined);

/** A project is full-bleed, outside the workspace sidebar: the shell (top bar + left icon rail)
 *  wraps the canvas, metrics, logs and settings pages. `?env=` is shared by all of them. */
const projectRoute = createRoute({
  getParentRoute: () => appRoute,
  path: "/projects/$projectId",
  validateSearch: (s: Record<string, unknown>): ProjectSearch => ({
    env: str(s.env),
    service: str(s.service),
    tab: tabs.find((t) => t === s.tab),
    since: ms(s.since),
    until: ms(s.until),
    deployment: str(s.deployment),
    // Back from "Connect GitHub" in the New Service dialog.
    add: addStepFrom(s.add),
    repo: typeof s.repo === "string" && githubRepoFromLink(`github.com/${s.repo}`) === s.repo ? s.repo : undefined,
    github: str(s.github),
  }),
  component: function Project() {
    const { projectId } = projectRoute.useParams();
    const { env } = projectRoute.useSearch();
    return <ProjectShell projectId={projectId} envId={env} />;
  },
});

const projectCanvasRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: "/",
  component: function ProjectCanvas() {
    const { projectId } = projectCanvasRoute.useParams();
    const search = projectCanvasRoute.useSearch();
    const navigate = useNavigate({ from: projectCanvasRoute.fullPath });
    return (
      <Suspense fallback={null}>
        <ProjectPage projectId={projectId} search={search} setSearch={(s) => void navigate({ search: s })} />
      </Suspense>
    );
  },
});
const projectMetricsRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: "/metrics",
  component: ProjectObservability,
});
const projectLogsRoute = createRoute({ getParentRoute: () => projectRoute, path: "/logs", component: LogsPage });
const projectSettingsIndexRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: "/settings",
  beforeLoad: ({ params, search }) => {
    throw redirect({ to: "/projects/$projectId/settings/$section", params: { projectId: params.projectId, section: "general" }, search: { env: search.env } });
  },
});
const projectSettingsRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: "/settings/$section",
  component: function ProjectSettings() {
    const { projectId, section } = projectSettingsRoute.useParams();
    return <ProjectSettingsPage projectId={projectId} section={section} />;
  },
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  setupRoute,
  loginRoute,
  twoFactorRoute,
  inviteRoute,
  cliLoginRoute,
  appRoute.addChildren([domainOnboardingRoute, workspaceRoute.addChildren([projectsRoute, newProjectRoute, settingsRoute, securityRoute, serversRoute, serverRoute]), projectRoute.addChildren([projectCanvasRoute, projectMetricsRoute, projectLogsRoute, projectSettingsIndexRoute, projectSettingsRoute]),
  ]),
]);

export function makeRouter(queryClient: QueryClient) {
  return createRouter({ routeTree, context: { queryClient }, defaultPreload: "intent", defaultErrorComponent: RouteError });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof makeRouter>;
  }
}
