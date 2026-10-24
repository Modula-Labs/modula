import { QueryClient } from "@tanstack/react-query";
import { Suspense, lazy } from "react";
import { Outlet, createRootRouteWithContext, createRoute, createRouter, redirect, useNavigate } from "@tanstack/react-router";
import { RouteError, Toaster } from "./components/ErrorBoundary";
import { SudoDialog } from "./components/SudoDialog";
import { WorkspaceShell } from "./components/shell/WorkspaceShell";
import { ApiError, api } from "./lib/api";
import { AccountSecurityPage } from "./pages/AccountSecurity";
import { DomainOnboardingPage } from "./pages/DomainOnboarding";
import { LoginPage } from "./pages/Login";
import { ProjectsPage } from "./pages/Projects";
import { ServersPage } from "./pages/Servers";
import { WorkspaceSettingsPage } from "./pages/WorkspaceSettings";
import type { ProjectSearch } from "./pages/project/ProjectPage";
import type { ServiceTab } from "./pages/project/ServiceSheet";
import { LogsPage } from "./pages/project/LogsPage";
import { ProjectObservability } from "./pages/project/ProjectObservability";
import { ProjectSettingsPage } from "./pages/project/ProjectSettingsPage";
import { ProjectShell } from "./pages/project/ProjectShell";
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

const loginRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login",
  validateSearch: (s: Record<string, unknown>): { expired?: boolean } => (s.expired ? { expired: true } : {}),
  beforeLoad: async ({ context }) => {
    if ((await instance(context.queryClient)).setupRequired) throw redirect({ to: "/setup" });
  },
  component: function Login() {
    const { expired } = loginRoute.useSearch();
    return <LoginPage notice={expired ? "Your session ended. Sign in again." : undefined} />;
  },
});

const twoFactorRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/login/2fa",
  component: TwoFactorPage,
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
const securityRoute = createRoute({ getParentRoute: () => workspaceRoute, path: "/account/security", component: AccountSecurityPage });
const settingsRoute = createRoute({ getParentRoute: () => workspaceRoute, path: "/settings", component: WorkspaceSettingsPage });
const serversRoute = createRoute({ getParentRoute: () => workspaceRoute, path: "/servers", component: ServersPage });

// The canvas (React Flow) is the heaviest part of the app; load it on demand.
const ProjectPage = lazy(() => import("./pages/project/ProjectPage").then((m) => ({ default: m.ProjectPage })));

const tabs: ServiceTab[] = ["deployments", "cron", "metrics", "database", "bucket", "variables", "backups", "settings"];
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
    add: s.add === "repo" ? "repo" : undefined,
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
  appRoute.addChildren([domainOnboardingRoute, workspaceRoute.addChildren([projectsRoute, settingsRoute, securityRoute, serversRoute]), projectRoute.addChildren([projectCanvasRoute, projectMetricsRoute, projectLogsRoute, projectSettingsIndexRoute, projectSettingsRoute]),
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
