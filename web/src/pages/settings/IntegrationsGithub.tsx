import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { BrandGithub } from "@/components/ui/icons";
import { Input } from "@/components/ui/Input";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { ApiError } from "@/lib/api";
import { SETTINGS_RETURN_PATH, connectGithub, connectHelp, connectMode, github, githubReturnNotice } from "@/lib/github";
import { useMe } from "@/lib/workspace";

/** The instance's GitHub App and this workspace's installations of it. */
export function useGithub(workspaceId: string) {
  const app = useQuery({ queryKey: ["github-app"], queryFn: github.app, staleTime: 60_000, enabled: !!workspaceId });
  const installations = useQuery({
    queryKey: ["github-installations", workspaceId],
    queryFn: () => github.installations(workspaceId),
    enabled: !!workspaceId && !!app.data?.configured,
    staleTime: 60_000,
    // Viewers can't list them, and older servers don't have the endpoint.
    retry: (n, e) => !(e instanceof ApiError && (e.status === 404 || e.status === 403)) && n < 2,
  });
  return { app, installations, connected: (installations.data?.length ?? 0) > 0 };
}

/** "Connect GitHub" for this user and workspace: whether they can, who to ask otherwise, and
 *  the one-click flow (creating the App first when the instance has none). */
export function useConnectGithub(workspaceId: string, returnTo: string) {
  const { data: me } = useMe();
  const { app } = useGithub(workspaceId);
  const appConfigured = !!app.data?.configured;
  const isInstanceAdmin = !!me?.user.isInstanceAdmin;
  const role = me?.workspaces.find((w) => w.id === workspaceId)?.role;
  const mode = connectMode({ appConfigured, isInstanceAdmin, role });
  const connect = useMutation({
    mutationFn: (org?: string) => connectGithub({ workspaceId, returnTo, appConfigured, org }),
  });
  return {
    /** Whether the instance has an App isn't known yet: don't offer or refuse anything. */
    loading: app.isPending,
    mode,
    help: mode === "connect" ? null : connectHelp[mode],
    /** The instance has no App yet and this user will create it (may pick an organization). */
    creatingApp: mode === "connect" && !appConfigured,
    connect,
    // The browser is leaving for GitHub once the request succeeds.
    busy: connect.isPending || connect.isSuccess,
  };
}

/** Workspace Settings → Integrations → GitHub. */
export function GithubIntegration({ workspaceId, isAdmin }: { workspaceId: string; isAdmin: boolean }) {
  const qc = useQueryClient();
  const { data: me } = useMe();
  const { app, installations } = useGithub(workspaceId);
  const c = useConnectGithub(workspaceId, SETTINGS_RETURN_PATH);
  const [notice, setNotice] = useState<ReturnType<typeof githubReturnNotice>>(null);
  const [useOrg, setUseOrg] = useState(false);
  const [org, setOrg] = useState("");

  // Back from GitHub (`/settings?github=installed`): say what happened, once.
  useEffect(() => {
    const url = new URL(window.location.href);
    const n = githubReturnNotice(url.searchParams.get("github"));
    if (!url.searchParams.has("github")) return;
    url.searchParams.delete("github");
    window.history.replaceState(window.history.state, "", url.pathname + url.search + url.hash);
    setNotice(n);
    void qc.invalidateQueries({ queryKey: ["github-app"] });
    void qc.invalidateQueries({ queryKey: ["github-installations"] });
  }, [qc]);
  // Bring the card into view once it renders (back from GitHub, or a "Connect GitHub" link).
  const ready = !!app.data;
  useEffect(() => {
    if (ready && (notice || window.location.hash === "#integrations-github"))
      document.getElementById("integrations-github")?.scrollIntoView({ block: "center" });
  }, [ready, notice]);

  if (app.isPending)
    return (
      <div className="p-4">
        <Skeleton className="h-10" />
      </div>
    );
  if (app.error)
    return (
      <div className="p-4">
        <Alert>{app.error.message}</Alert>
      </div>
    );

  const a = app.data;
  const list = installations.data ?? [];
  const forbidden = installations.error instanceof ApiError && installations.error.status === 403;
  const isInstanceAdmin = !!me?.user.isInstanceAdmin;
  const localUrl = /^https?:\/\/(localhost|127\.|\[::1\])/.test(a.webhookUrl);

  return (
    <div id="integrations-github" className="flex flex-col gap-3 px-4 py-3">
      <div className="flex min-h-10 flex-wrap items-center gap-3">
        <BrandGithub className="size-5 text-muted-foreground" />
        <div className="min-w-[14rem] flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-medium">GitHub</span>
            {list.length ? <Badge tone="success">Connected</Badge> : null}
          </div>
          <div className="text-sm text-muted-foreground">
            {forbidden
              ? "Visible to workspace members and admins."
              : list.length
                ? "Repositories from these accounts show up when you add a service. Pushes deploy automatically."
                : (c.help ?? "Deploy private repositories, and redeploy on every push.")}
          </div>
        </div>
        {c.mode === "connect" ? (
          <Button
            className="ml-auto shrink-0"
            size="sm"
            variant="secondary"
            icon={<BrandGithub />}
            loading={c.busy}
            onClick={() => c.connect.mutate(useOrg ? org : undefined)}
          >
            {list.length ? "Add account or organization" : "Connect GitHub"}
          </Button>
        ) : null}
      </div>
      {notice ? <Alert tone={notice.tone}>{notice.text}</Alert> : null}
      {c.connect.error ? <Alert>{c.connect.error.message}</Alert> : null}
      {installations.error && !forbidden ? <Alert tone="warn">{installations.error.message}</Alert> : null}
      {c.creatingApp ? (
        <div className="flex flex-col gap-2 pl-8">
          <label className="flex w-fit items-center gap-2 text-xs text-muted-foreground">
            <input type="checkbox" className="size-3.5 accent-foreground" checked={useOrg} onChange={(e) => setUseOrg(e.target.checked)} />
            Create the GitHub App under an organization
          </label>
          {useOrg ? (
            <div className="flex max-w-sm flex-col gap-1">
              <Input
                mono
                aria-label="GitHub organization"
                placeholder="acme"
                autoComplete="off"
                value={org}
                onChange={(e) => setOrg(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                A private App can only be installed on the account that owns it, so pick the organization whose
                repositories you deploy.
              </p>
            </div>
          ) : null}
          {localUrl ? (
            <Alert tone="warn">
              This instance's public URL is local, so GitHub can't deliver push webhooks. Set the public URL first if
              you want deploys on push.
            </Alert>
          ) : null}
        </div>
      ) : null}
      {a.configured && installations.isPending && !forbidden ? <Skeleton className="h-8" /> : null}
      {list.length ? (
        <ul className="flex flex-col gap-1 pl-8">
          {list.map((i) => (
            <li key={i.id} className="flex min-h-9 items-center gap-2.5">
              <img src={i.avatarUrl} alt="" className="size-6 rounded-full border border-border" />
              <span className="text-sm font-medium">{i.accountLogin}</span>
              <span className="text-xs text-muted-foreground">
                {i.accountType === "Organization" ? "Organization" : "Personal account"}
              </span>
              {isAdmin ? (
                <a
                  href={i.configureUrl}
                  target="_blank"
                  rel="noreferrer"
                  className="ml-auto text-sm text-muted-foreground underline underline-offset-4 hover:text-foreground"
                >
                  Configure on GitHub
                </a>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}
      {a.configured && a.htmlUrl && isInstanceAdmin ? (
        <div className="pl-8 text-xs text-muted-foreground">
          App{" "}
          <a href={a.htmlUrl} target="_blank" rel="noreferrer" className="underline underline-offset-4">
            {a.slug}
          </a>
          {a.ownerLogin ? ` owned by ${a.ownerLogin}` : ""} · webhooks to <span className="font-mono">{a.webhookUrl}</span>
        </div>
      ) : null}
    </div>
  );
}
