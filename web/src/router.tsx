import { QueryClient } from "@tanstack/react-query";
import { Outlet, createRootRouteWithContext, createRoute, createRouter, redirect } from "@tanstack/react-router";
import { SudoDialog } from "./components/SudoDialog";
import { WorkspaceShell } from "./components/shell/WorkspaceShell";
import { ApiError, api } from "./lib/api";
import { AccountSecurityPage } from "./pages/AccountSecurity";
import { LoginPage } from "./pages/Login";
import { ProjectsPage } from "./pages/Projects";
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
  component: WorkspaceShell,
});

const projectsRoute = createRoute({ getParentRoute: () => appRoute, path: "/projects", component: ProjectsPage });
const securityRoute = createRoute({ getParentRoute: () => appRoute, path: "/account/security", component: AccountSecurityPage });

const routeTree = rootRoute.addChildren([
  indexRoute,
  setupRoute,
  loginRoute,
  twoFactorRoute,
  appRoute.addChildren([projectsRoute, securityRoute]),
]);

export function makeRouter(queryClient: QueryClient) {
  return createRouter({ routeTree, context: { queryClient }, defaultPreload: "intent" });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof makeRouter>;
  }
}
