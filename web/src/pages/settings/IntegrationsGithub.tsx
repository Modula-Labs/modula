import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { BrandGithub } from "@/components/ui/icons";
import { Card, CardContent } from "@/components/ui/Card";
import { Plus, Trash } from "@/components/ui/icons";
import { Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { ApiError } from "@/lib/api";
import {
  SETTINGS_RETURN_PATH,
  connectGithub,
  appOwner,
  connectHelp,
  connectMode,
  connectStep,
  github,
  githubAvatar,
  githubReturnNotice,
  publicApp,
  type GithubApp,
} from "@/lib/github";
import { safeHttpUrl } from "@/lib/url";
import { useMe } from "@/lib/workspace";

/** The instance's GitHub Apps and this workspace's installations of them. */
export function useGithub(workspaceId: string) {
  const apps = useQuery({
    queryKey: ["github-apps"],
    queryFn: github.apps,
    staleTime: 60_000,
    enabled: !!workspaceId,
  });
  const installations = useQuery({
    queryKey: ["github-installations", workspaceId],
    queryFn: () => github.installations(workspaceId),
    enabled: !!workspaceId && (apps.data?.apps.length ?? 0) > 0,
    staleTime: 60_000,
    // Viewers can't list them (403): don't retry.
    retry: (n, e) => !(e instanceof ApiError && e.status === 403) && n < 2,
  });
  return {
    apps,
    installations,
    connected: (installations.data?.length ?? 0) > 0,
  };
}

/** "Connect GitHub" for this user and workspace: whether they can, who to ask otherwise, and
 *  the flows: install one of the instance's Apps, or create the instance's App first. */
export function useConnectGithub(workspaceId: string, returnTo: string) {
  const { data: me } = useMe();
  const { apps } = useGithub(workspaceId);
  const list = apps.data?.apps ?? [];
  const isInstanceAdmin = !!me?.user.isInstanceAdmin;
  const role = me?.workspaces.find((w) => w.id === workspaceId)?.role;
  const mode = connectMode({
    appCount: list.length,
    isInstanceAdmin,
    role,
  });
  const connect = useMutation({
    /** `appId`: install that App; otherwise create a new App (`org`, else personal). */
    mutationFn: (o: { appId?: number; org?: string }) =>
      connectGithub({ workspaceId, returnTo, ...o }),
  });
  return {
    /** Whether the instance has an App isn't known yet: don't offer or refuse anything. */
    loading: apps.isPending,
    mode,
    help: mode === "connect" ? null : connectHelp[mode],
    apps: list,
    step: connectStep(list.length),
    /** The public App, when there is one: it installs on any account (picked on GitHub). */
    publicApp: publicApp(list),
    /** Instance admins create the App; once a public one exists, there's nothing to add. */
    canAddApp: mode === "connect" && isInstanceAdmin && !publicApp(list),
    isInstanceAdmin,
    connect,
    // The browser is leaving for GitHub once the request succeeds.
    busy: connect.isPending || connect.isSuccess,
  };
}

/** Creates the instance's GitHub App under the admin's account, then installs it: GitHub's own
 *  page offers their accounts and organizations to pick from, so nothing has to be typed. Under
 *  "Advanced", the App can be owned by an organization instead (`knownOrgs` are suggested). */
export function AddAppModal({
  open,
  onOpenChange,
  onSubmit,
  busy,
  error,
  localUrl,
  knownOrgs = [],
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSubmit: (org?: string) => void;
  busy: boolean;
  error?: string | null;
  localUrl?: boolean;
  /** Organizations already connected, offered for the advanced "owned by an organization". */
  knownOrgs?: string[];
}) {
  const [advanced, setAdvanced] = useState(false);
  const [org, setOrg] = useState("");
  const asOrg = advanced && !!org.trim();
  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      title="Connect GitHub"
      description="You'll pick which accounts and repositories Modula can see on GitHub."
      footer={
        <>
          <Button
            type="button"
            variant="ghost"
            onClick={() => onOpenChange(false)}
          >
            Cancel
          </Button>
          <Button
            type="submit"
            form="add-github-app"
            variant="primary"
            icon={<BrandGithub />}
            loading={busy}
          >
            Continue to GitHub
          </Button>
        </>
      }
    >
      <form
        id="add-github-app"
        className="flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          // Portal events bubble to React parents: don't submit a form this modal opens from.
          e.stopPropagation();
          onSubmit(asOrg ? org.trim() : undefined);
        }}
      >
        <p className="text-sm text-muted-foreground">
          GitHub first shows the App Modula creates for this instance under
          your account; confirm it. You can install it on more accounts and
          organizations later.
        </p>
        <details
          className="text-sm"
          open={advanced}
          onToggle={(e) => setAdvanced(e.currentTarget.open)}
        >
          <summary className="cursor-pointer text-muted-foreground hover:text-foreground">
            Advanced: own the App by an organization
          </summary>
          <div className="mt-2 flex flex-col gap-2">
            <p className="text-muted-foreground">
              Only needed if the App must not belong to your personal account
              (for example, so it stays when you leave). You need permission
              to register GitHub Apps in the organization.
            </p>
            <Input
              mono
              aria-label="GitHub organization"
              placeholder="organization"
              autoComplete="off"
              list={knownOrgs.length ? "github-known-orgs" : undefined}
              value={org}
              onChange={(e) => setOrg(e.target.value)}
            />
            {knownOrgs.length ? (
              <datalist id="github-known-orgs">
                {knownOrgs.map((o) => (
                  <option key={o} value={o} />
                ))}
              </datalist>
            ) : null}
          </div>
        </details>
        {localUrl ? (
          <Alert tone="warn">
            This instance's public URL is local, so GitHub can't deliver push
            webhooks. Set the public URL first if you want deploys on push.
          </Alert>
        ) : null}
        {error ? <Alert>{error}</Alert> : null}
      </form>
    </Modal>
  );
}

/** Organizations the workspace already connected (suggested as an App's owner). */
const orgsOf = (list: { accountLogin: string; accountType: string }[]) =>
  list.filter((i) => i.accountType === "Organization").map((i) => i.accountLogin);

const isLocalUrl = (u: string | undefined) =>
  !!u && /^https?:\/\/(localhost|127\.|\[::1\])/.test(u);

/** Workspace Settings → Integrations → GitHub. */
export function GithubIntegration({
  workspaceId,
  isAdmin,
}: {
  workspaceId: string;
  isAdmin: boolean;
}) {
  const qc = useQueryClient();
  const { apps, installations } = useGithub(workspaceId);
  const c = useConnectGithub(workspaceId, SETTINGS_RETURN_PATH);
  const [notice, setNotice] =
    useState<ReturnType<typeof githubReturnNotice>>(null);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<GithubApp | null>(null);
  const remove = useMutation({
    mutationFn: (id: number) => github.deleteApp(id),
    onSuccess: () => {
      setRemoving(null);
      void qc.invalidateQueries({ queryKey: ["github-apps"] });
      void qc.invalidateQueries({ queryKey: ["github-installations"] });
      void qc.invalidateQueries({ queryKey: ["github-repos"] });
    },
  });

  // Back from GitHub (`/settings?github=installed`): say what happened, once.
  useEffect(() => {
    const url = new URL(window.location.href);
    const n = githubReturnNotice(url.searchParams.get("github"));
    if (!url.searchParams.has("github")) return;
    url.searchParams.delete("github");
    window.history.replaceState(
      window.history.state,
      "",
      url.pathname + url.search + url.hash,
    );
    setNotice(n);
    void qc.invalidateQueries({ queryKey: ["github-apps"] });
    void qc.invalidateQueries({ queryKey: ["github-installations"] });
  }, [qc]);
  // Bring the card into view once it renders (back from GitHub, or a "Connect GitHub" link).
  const ready = !!apps.data;
  useEffect(() => {
    if (ready && (notice || window.location.hash === "#integrations-github"))
      document
        .getElementById("integrations-github")
        ?.scrollIntoView({ block: "center" });
  }, [ready, notice]);

  if (apps.isPending)
    return (
      <Card size="sm">
        <CardContent>
          <Skeleton className="h-10" />
        </CardContent>
      </Card>
    );
  if (apps.error)
    return (
      <Card size="sm">
        <CardContent>
          <Alert>{apps.error.message}</Alert>
        </CardContent>
      </Card>
    );

  const appList = apps.data.apps;
  const list = installations.data ?? [];
  const forbidden =
    installations.error instanceof ApiError &&
    installations.error.status === 403;
  const localUrl = isLocalUrl(apps.data.webhookUrl);

  return (
    <div id="integrations-github">
      <Card size="sm">
        <CardContent className="flex flex-col gap-3">
          <div className="flex min-h-10 flex-wrap items-center gap-3">
            <BrandGithub className="size-5 text-muted-foreground" />
            <div className="min-w-[14rem] flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-bold">GitHub</span>
                {list.length ? <Badge tone="success">Connected</Badge> : null}
              </div>
              <div className="text-sm text-muted-foreground">
                {forbidden
                  ? "Visible to workspace members and admins."
                  : list.length
                    ? "Repositories from these accounts show up when you add a service. Pushes deploy automatically."
                    : (c.help ??
                      "Deploy private repositories, and redeploy on every push.")}
              </div>
            </div>
            {c.canAddApp ? (
              <Button
                className="ml-auto shrink-0"
                variant="secondary"
                icon={appList.length ? <Plus /> : <BrandGithub />}
                onClick={() => {
                  c.connect.reset();
                  setAdding(true);
                }}
              >
                {appList.length ? "Add GitHub App" : "Connect GitHub"}
              </Button>
            ) : null}
          </div>
          {notice ? <Alert tone={notice.tone}>{notice.text}</Alert> : null}
          {!adding && c.connect.error ? (
            <Alert>{c.connect.error.message}</Alert>
          ) : null}
          {installations.error && !forbidden ? (
            <Alert tone="warn">{installations.error.message}</Alert>
          ) : null}
          {installations.isPending && appList.length && !forbidden ? (
            <Skeleton className="h-8" />
          ) : null}
          {appList.map((a) => (
            <MissingPermissions key={a.id} app={a} />
          ))}
          {list.length ? (
            <ul className="flex flex-col gap-1 pl-8">
              {list.map((i) => (
                <li key={i.id} className="flex min-h-9 items-center gap-2.5">
                  <img
                    src={i.avatarUrl}
                    alt=""
                    className="size-6 rounded-full border border-border"
                  />
                  <span className="text-sm">{i.accountLogin}</span>
                  <span className="text-[13px] text-muted-foreground">
                    {i.accountType === "Organization"
                      ? "Organization"
                      : "Personal account"}
                    {i.appSlug ? ` · via ${i.appSlug}` : ""}
                  </span>
                  {isAdmin && safeHttpUrl(i.configureUrl, true) ? (
                    <a
                      href={safeHttpUrl(i.configureUrl, true)}
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
          {appList.length ? (
            <div className="flex flex-col gap-1 pl-8">
              <div className="text-[13px] font-bold text-muted-foreground">
                GitHub Apps
              </div>
              <ul className="flex flex-col gap-1">
                {appList.map((a) => {
                  const owner = appOwner(a);
                  const href = safeHttpUrl(a.htmlUrl, true);
                  return (
                    <li
                      key={a.id}
                      className="flex min-h-9 items-center gap-2.5"
                    >
                      <img
                        src={githubAvatar(owner)}
                        alt=""
                        className="size-6 rounded-full border border-border"
                      />
                      <span className="text-sm">{owner}</span>
                      <Badge>{a.public ? "Any account" : "Private"}</Badge>
                      {href ? (
                        <a
                          href={href}
                          target="_blank"
                          rel="noreferrer"
                          className="text-[13px] text-muted-foreground underline underline-offset-4 hover:text-foreground"
                        >
                          {a.slug}
                        </a>
                      ) : null}
                      <div className="ml-auto flex items-center gap-1.5">
                        {c.mode === "connect" ? (
                          <Button
                            size="sm"
                            variant="secondary"
                            loading={c.busy}
                            onClick={() => c.connect.mutate({ appId: a.id })}
                          >
                            {a.public ? "Install on another account" : "Install"}
                          </Button>
                        ) : null}
                        {c.isInstanceAdmin ? (
                          <Button
                            size="icon-sm"
                            variant="ghost"
                            aria-label={`Remove ${a.slug}`}
                            title="Remove App"
                            className="text-muted-foreground"
                            onClick={() => {
                              remove.reset();
                              setRemoving(a);
                            }}
                          >
                            <Trash />
                          </Button>
                        ) : null}
                      </div>
                    </li>
                  );
                })}
              </ul>
              {c.isInstanceAdmin ? (
                <div className="text-[13px] text-muted-foreground">
                  Webhooks to{" "}
                  <span className="font-mono">{apps.data.webhookUrl}</span>
                </div>
              ) : null}
            </div>
          ) : null}
        </CardContent>
      </Card>
      <AddAppModal
        open={adding}
        onOpenChange={setAdding}
        busy={c.busy}
        error={c.connect.error?.message}
        localUrl={localUrl}
        knownOrgs={orgsOf(list)}
        onSubmit={(org) => c.connect.mutate({ org })}
      />
      <Modal
        open={!!removing}
        onOpenChange={(o) => !o && setRemoving(null)}
        title="Remove GitHub App?"
        description={
          removing
            ? `${removing.slug} is forgotten by Modula and its installations are disconnected from every workspace. Services keep running but stop deploying on push. The App itself stays on GitHub; delete it there too if you no longer need it.`
            : undefined
        }
        footer={
          <>
            <Button variant="ghost" onClick={() => setRemoving(null)}>
              Cancel
            </Button>
            <Button
              variant="danger"
              loading={remove.isPending}
              onClick={() => removing && remove.mutate(removing.id)}
            >
              Remove App
            </Button>
          </>
        }
      >
        {remove.error ? <Alert>{remove.error.message}</Alert> : null}
      </Modal>
    </div>
  );
}

/** An App created by an older version lacks permissions or events newer features need (pull
 *  request previews, waiting for CI): its owner approves them on GitHub. */
function MissingPermissions({ app }: { app: GithubApp }) {
  const missing = [...app.missingPermissions, ...app.missingEvents.map((e) => `${e} events`)];
  if (!missing.length) return null;
  const href = safeHttpUrl(app.permissionsUrl, true);
  return (
    <Alert tone="warn" title={`${appOwner(app)}'s GitHub App needs new permissions`}>
      Pull request previews and waiting for CI need {missing.join(", ")}. The App's owner approves them on GitHub,
      then each installation accepts them.{" "}
      {href ? (
        <a href={href} target="_blank" rel="noreferrer" className="underline underline-offset-4">
          Review permissions on GitHub
        </a>
      ) : null}
    </Alert>
  );
}
