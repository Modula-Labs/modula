import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { clsx } from "clsx";
import { useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Activity, AlertCircle, BrandDocker, BrandGithub, Bucket, Copy, Database, Disk, LayoutGrid, Plus, Power, Refresh, Rocket, Trash, Zap } from "@/components/ui/icons";
import { ContextMenu, MenuItem, MenuLabel, MenuSeparator } from "@/components/ui/Menu";
import { useLingering } from "@/components/ui/Sheet";
import { api, type DeploymentStatus, type EnvWarning, type PendingService } from "@/lib/api";
import { requestFocus } from "@/lib/focus";
import { useTemplates } from "@/lib/useTemplates";
import { projectReturn } from "@/lib/github";
import { atLeast } from "@/lib/roles";
import { load, save } from "@/lib/storage";
import { templateStack } from "@/lib/templates";
import { ActivityPanel } from "./ActivityPanel";
import { Canvas, type CanvasPoint, type CanvasService } from "./Canvas";
import { DeploymentSheet } from "./DeploymentSheet";
import { NewServiceDialog, type NewServiceStep } from "./NewServiceDialog";
import { PendingBar, usePending } from "./PendingBar";
import { DeleteServiceDialog } from "./ProjectDialogs";
import { useProject } from "./ProjectContext";
import { SyncDialog } from "./SyncDialog";
import { ServiceSheet, type ServiceTab } from "./ServiceSheet";
import { WarningsPanel } from "./WarningsPanel";
import { warningTarget, warningsLabel, worstSeverity } from "./warnings";

export interface ProjectSearch {
  env?: string;
  service?: string;
  tab?: ServiceTab;
  /** Logs page: preselected time window, epoch milliseconds. */
  since?: number;
  until?: number;
  deployment?: string;
  /** Open the New Service dialog on this step (back from "Connect GitHub", or from New project). */
  add?: "repo" | "image" | "function" | "database" | "bucket" | "templates";
  /** With `add=repo`: the repository to prefill, `owner/name`. */
  repo?: string;
  /** How the round trip through GitHub went (`installed`, `requested`, …). */
  github?: string;
}

/** The project view: canvas of services in one environment, with the service
 *  and deployment sheets layered on top. All selection lives in the URL. */
export function ProjectPage({
  projectId,
  search,
  setSearch,
}: {
  projectId: string;
  search: ProjectSearch;
  setSearch: (s: ProjectSearch) => void;
}) {
  const qc = useQueryClient();
  const [adding, setAdding] = useState(false);
  const [addStep, setAddStep] = useState<NewServiceStep>("root");
  // Shown in the reopened dialog after "Connect GitHub" (the canvas toast would sit under it).
  const [githubNotice, setGithubNotice] = useState<ReturnType<typeof projectReturn>["notice"]>(null);
  // Where a service added from the canvas menu should go.
  const [initialRepo, setInitialRepo] = useState<string | undefined>();
  const [placeAt, setPlaceAt] = useState<{ x: number; y: number } | null>(null);
  const [menu, setMenu] = useState<{ at: CanvasPoint; serviceId?: string } | null>(null);
  const [deletingService, setDeletingService] = useState<{ id: string; name: string }>();
  const [notice, setNotice] = useState<string | null>(null);
  const [syncing, setSyncing] = useState(false);
  // An explicit open/close is remembered in this browser. Until there is one, the panel follows
  // the project: closed while it has no activity, open once it does.
  const [activityPref, setActivityPref] = useState<boolean | null>(() => {
    const v = load("mc.activity.open");
    return v === "1" ? true : v === "0" ? false : null;
  });
  const { project: p, env, role } = useProject();
  const hasActivity = useQuery({
    // Same key as the panel's unfiltered query, so they share one fetch.
    queryKey: ["activity", projectId, env?.id, "", ""],
    queryFn: () => api.activity(projectId, { environmentId: env!.id }),
    enabled: activityPref === null && !!env,
    retry: false,
    select: (d) => d.items.length > 0,
  });
  const activityOpen = activityPref ?? hasActivity.data ?? false;
  const setActivityOpen = (open: boolean | ((o: boolean) => boolean)) => {
    const next = typeof open === "function" ? open(activityOpen) : open;
    save("mc.activity.open", next ? "1" : "0");
    setActivityPref(next);
  };

  const pending = usePending(env?.id).data;
  // Current problems in this environment; the button only shows while there are some.
  const navigate = useNavigate();
  const [warningsOpen, setWarningsOpen] = useState(false);
  const warnings = useQuery({
    queryKey: ["warnings", env?.id],
    queryFn: () => api.warnings(env!.id),
    enabled: !!env,
    refetchInterval: 15_000,
    retry: false,
  });
  const warningItems = warnings.data?.items ?? [];
  const worst = worstSeverity(warningItems);
  const openWarning = (w: EnvWarning) => {
    const t = warningTarget(w);
    if (!t) return;
    if (t.kind === "service") {
      if (t.focus) requestFocus(t.focus.instanceId, t.focus.target);
      setSearch({ env: search.env, service: t.serviceId, tab: t.tab, deployment: t.deploymentId });
    } else if (t.kind === "server") void navigate({ to: "/servers/$nodeId", params: { nodeId: t.nodeId } } as never);
    else if (t.kind === "servers") void navigate({ to: "/servers" } as never);
    else
      void navigate({
        to: "/projects/$projectId/settings/$section",
        params: { projectId, section: "shared-variables" },
        search: { env: env?.id },
      } as never);
  };
  const services: CanvasService[] = useMemo(() => {
    if (!p || !env) return [];
    return p.services.map((s) => {
      const i = p.instances.find((x) => x.serviceId === s.id && x.environmentId === env.id);
      return {
        id: s.id,
        name: s.name,
        domain: i?.domain ?? undefined,
        repo: i?.sourceType === "repo" ? (i.repo ?? undefined) : undefined,
        status: (i?.latestDeploymentStatus ?? null) as DeploymentStatus | null,
        degraded: i?.degraded,
        stack: templateStack(s.template ?? i?.template) ?? i?.stack,
        volume: i?.volume?.mountPath ?? null,
        image: i?.sourceType === "image" ? i.image : undefined,
        dependsOn: i?.dependsOn,
        ...pendingOf(pending?.services.find((x) => x.serviceId === s.id)),
        cron: i?.cronSchedule ? { nextAt: i.cronNextAt } : undefined,
        r2: s.template === "r2" ? { bucket: i?.bucket ?? null } : undefined,
      };
    });
  }, [p, env, pending]);
  const currentInstanceId = p?.instances.find((i) => i.serviceId === search.service && i.environmentId === env?.id)?.id;
  // Keep the sheets mounted while they slide out.
  const { value: instanceId, leaving: sheetLeaving } = useLingering(currentInstanceId);
  const { value: deploymentId, leaving: deploymentLeaving } = useLingering(currentInstanceId ? search.deployment : undefined);
  const canEdit = atLeast(role, "member");
  const canDeploy = atLeast(role, "deployer");
  // While the "Apply N changes" bar is up it owns the screen's primary button.
  const hasPending = canDeploy && (pending?.count ?? 0) > 0;
  // Same query as the add dialog; the canvas menu offers Database only when there are templates.
  const templates = useTemplates(canEdit);
  useEffect(() => {
    if (!notice) return;
    const t = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(t);
  }, [notice]);
  // New services wait as green cards in the pending changes; the canvas stays in view.
  const onCreated = () => {
    if (env) void qc.invalidateQueries({ queryKey: ["pending", env.id] });
  };
  const openAdd = (step: NewServiceStep, at?: CanvasPoint) => {
    setAddStep(step);
    setPlaceAt(at?.flow ?? null);
    setAdding(true);
  };
  // Back from "Connect GitHub": say how it went, reopen the dialog on the GitHub step (its
  // repository combobox takes focus), and drop the one-shot parameters from the URL.
  useEffect(() => {
    if (!search.add && !search.github) return;
    const back = projectReturn(search);
    void qc.invalidateQueries({ queryKey: ["github-apps"] });
    void qc.invalidateQueries({ queryKey: ["github-installations"] });
    if (back.reopen) {
      setGithubNotice(back.notice);
      setInitialRepo(search.repo);
      openAdd("repo");
    } else if (search.add) {
      openAdd(search.add);
    } else if (back.notice) setNotice(back.notice.text);
    setSearch({ ...search, add: undefined, github: undefined, repo: undefined });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [search.add, search.github]);
  const instanceOf = (serviceId: string) => p?.instances.find((i) => i.serviceId === serviceId && i.environmentId === env?.id);
  const run = (what: string, action: () => Promise<unknown>) => {
    void action()
      .then(() => {
        setNotice(what);
        return qc.invalidateQueries({ queryKey: ["project", projectId] });
      })
      .catch((e: Error) => setNotice(e.message));
  };

  return (
    <div className="relative h-full">
        {p && env ? (
          <Canvas
            projectId={projectId}
            services={services}
            selected={search.service}
            onSelect={(id) => {
              setSearch({ env: search.env, service: id, tab: id ? search.tab : undefined });
            }}
            onPaneMenu={canEdit ? (at) => setMenu({ at }) : undefined}
            onServiceMenu={(id, at) => setMenu({ at, serviceId: id })}
            onViewportChange={() => setMenu(null)}
            placeNew={placeAt}
          />
        ) : (
          <div className="dotgrid h-full" />
        )}
        {p && env ? (
          <div className="absolute top-3 right-3 z-10 flex items-center gap-2">
            {warningItems.length > 0 ? (
              <Button
                variant="outline"
                icon={
                  <AlertCircle
                    className={clsx(
                      worst === "error" ? "text-destructive" : worst === "warning" ? "text-badge-amber-fg" : undefined,
                    )}
                  />
                }
                aria-pressed={warningsOpen}
                onClick={() => setWarningsOpen((o) => !o)}
              >
                {warningsLabel(warningItems.length)}
              </Button>
            ) : null}
            <Button
              variant="outline"
              icon={<Activity />}
              aria-pressed={activityOpen && !warningsOpen}
              onClick={() => {
                if (warningsOpen) {
                  setWarningsOpen(false);
                  setActivityOpen(true);
                } else setActivityOpen((o) => !o);
              }}
            >
              Activity
            </Button>
            {canEdit ? (
              <>
                <Button
                  variant="outline"
                  icon={<Refresh />}
                  disabled={p.environments.length < 2}
                  title={p.environments.length < 2 ? "Create another environment to sync from" : undefined}
                  onClick={() => setSyncing(true)}
                >
                  Sync
                </Button>
                <Button variant={hasPending ? "outline" : "primary"} icon={<Plus />} onClick={() => openAdd("root")}>
                  Add
                </Button>
              </>
            ) : null}
          </div>
        ) : null}
        {p && env && canDeploy ? <PendingBar projectId={projectId} environmentId={env.id} canDiscard={canEdit} /> : null}
        {p && env ? (
          <ContextMenu at={menu?.at.screen ?? null} onClose={() => setMenu(null)} label={menu?.serviceId ? "Service" : "Add New Service"}>
            {menu && !menu.serviceId ? (
              <>
                <MenuLabel>Add New Service</MenuLabel>
                <MenuItem icon={<BrandGithub />} onSelect={() => openAdd("repo", menu.at)}>
                  GitHub Repo
                </MenuItem>
                {templates.data?.length ? (
                  <MenuItem icon={<Database />} onSelect={() => openAdd("database", menu.at)}>
                    Database
                  </MenuItem>
                ) : null}
                <MenuItem icon={<BrandDocker />} onSelect={() => openAdd("image", menu.at)}>
                  Docker Image
                </MenuItem>
                <MenuItem icon={<Zap />} onSelect={() => openAdd("function", menu.at)}>
                  Function
                </MenuItem>
                <MenuItem icon={<Bucket />} onSelect={() => openAdd("bucket", menu.at)}>
                  Bucket
                </MenuItem>

                {search.service && !instanceOf(search.service)?.volume ? (
                  <MenuItem
                    icon={<Disk />}
                    title="Attach a volume to the selected service"
                    onSelect={() => setSearch({ env: search.env, service: search.service, tab: "settings" })}
                  >
                    Volume
                  </MenuItem>
                ) : null}
              </>
            ) : null}
            {menu?.serviceId
              ? (() => {
                  const id = menu.serviceId;
                  const svc = p.services.find((s) => s.id === id);
                  const inst = instanceOf(id);
                  if (!svc) return null;
                  const pendingHere = (services.find((s) => s.id === id)?.pendingChanges ?? 0) > 0;
                  const canRestart = !!inst?.latestDeploymentId && ["active", "sleeping", "crashed"].includes(inst.latestDeploymentStatus ?? "");
                  return (
                    <>
                      <MenuLabel>{svc.name}</MenuLabel>
                      <MenuItem icon={<LayoutGrid />} onSelect={() => setSearch({ env: search.env, service: id, tab: search.tab })}>
                        Open
                      </MenuItem>
                      {canDeploy && inst ? (
                        <MenuItem
                          icon={<Rocket />}
                          onSelect={() => run(`Deploying ${svc.name}`, () => api.deploy(inst.id))}
                        >
                          {inst.latestDeploymentId && !pendingHere ? "Redeploy" : "Deploy"}
                        </MenuItem>
                      ) : null}
                      {canDeploy && canRestart && inst?.latestDeploymentId ? (
                        <MenuItem
                          icon={<Power />}
                          onSelect={() => run(`Restarting ${svc.name}`, () => api.restartDeployment(inst.latestDeploymentId!))}
                        >
                          Restart
                        </MenuItem>
                      ) : null}
                      <MenuItem
                        icon={<Copy />}
                        onSelect={() =>
                          run("Copied private domain", () => navigator.clipboard.writeText(`${svc.name}.modula.internal`))
                        }
                      >
                        Copy private domain
                      </MenuItem>
                      {canEdit ? (
                        <>
                          <MenuSeparator />
                          <MenuItem danger icon={<Trash />} onSelect={() => setDeletingService({ id, name: svc.name })}>
                            Delete service…
                          </MenuItem>
                        </>
                      ) : null}
                    </>
                  );
                })()
              : null}
          </ContextMenu>
        ) : null}
        {notice ? (
          <div role="status" className="anim-pop absolute bottom-4 left-1/2 z-10 -translate-x-1/2 rounded-xl border border-border bg-popover px-4 py-2.5 text-sm shadow-menu">
            {notice}
          </div>
        ) : null}
        {p && env && warningsOpen && !instanceId ? (
          <WarningsPanel items={warningItems} onOpen={openWarning} onClose={() => setWarningsOpen(false)} />
        ) : null}
        {p && env && activityOpen && !warningsOpen && !instanceId ? (
          <ActivityPanel
            projectId={projectId}
            environmentId={env.id}
            services={p.services}
            onClose={() => setActivityOpen(false)}
            onOpenDeployment={(service, deployment) => {
              if (service) setSearch({ env: search.env, service, tab: "deployments", deployment });
            }}
          />
        ) : null}
        {p && env && services.length === 0 && atLeast(role, "member") ? (
          // An empty project shows the picker itself, in place, nothing dimmed.
          <div className="pointer-events-none absolute inset-0 flex items-start justify-center px-4 pt-[12vh]">
            <div className="pointer-events-auto max-w-full">
              <NewServiceDialog
                key={adding ? addStep : "root"}
                inline
                open
                onOpenChange={(o) => {
                  if (!o) setAdding(false);
                }}
                initialStep={adding ? addStep : "root"}
                initialRepo={initialRepo}
                githubNotice={githubNotice}
                projectId={projectId}
                environmentId={env.id}
                onCreated={onCreated}
              />
            </div>
          </div>
        ) : p && services.length === 0 ? (
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
            <div className="pointer-events-auto flex flex-col items-center gap-3 rounded-xl border border-dashed border-border bg-card px-10 py-8 text-center">
              <div className="text-base font-bold">No services yet</div>
              <p className="max-w-xs text-sm text-muted-foreground">A Member of this workspace can add them.</p>
            </div>
          </div>
        ) : null}

        {instanceId ? (
          <ServiceSheet
            key={instanceId}
            instanceId={instanceId}
            role={role}
            tab={search.tab ?? "deployments"}
            deploymentId={search.deployment}
            leaving={sheetLeaving}
            onTab={(tab) => setSearch({ ...search, tab, deployment: undefined })}
            onOpenDeployment={(deployment) => setSearch({ ...search, tab: "deployments", deployment })}
            onClose={() => setSearch({ env: search.env })}
          />
        ) : null}
        {instanceId && deploymentId ? (
          <DeploymentSheet
            key={deploymentId}
            deploymentId={deploymentId}
            leaving={deploymentLeaving || sheetLeaving}
            onClose={() => setSearch({ ...search, deployment: undefined })}
          />
        ) : null}

      {p && env && services.length > 0 ? (
        <NewServiceDialog
          open={adding}
          onOpenChange={(o) => {
            setAdding(o);
            if (!o) {
              setPlaceAt(null);
              setGithubNotice(null);
              setInitialRepo(undefined);
            }
          }}
          initialStep={addStep}
          initialRepo={initialRepo}
          githubNotice={githubNotice}
          projectId={projectId}
          environmentId={env.id}
          onCreated={onCreated}
        />
      ) : null}
      {p && env ? (
        <SyncDialog open={syncing} onOpenChange={setSyncing} projectId={projectId} target={env} environments={p.environments} />
      ) : null}
      <DeleteServiceDialog
        service={deletingService}
        onClose={() => setDeletingService(undefined)}
        projectId={projectId}
        onDeleted={(id) => {
          if (search.service === id) setSearch({ env: search.env });
        }}
      />
    </div>
  );
}

/** A service's pending changes on its card: a new one counts its settings, not its own creation. */
function pendingOf(s: PendingService | undefined): Pick<CanvasService, "pendingChanges" | "isNew" | "newSummary"> {
  if (!s) return { pendingChanges: 0 };
  if (!s.new) return { pendingChanges: s.changes.length };
  const count = (kind: string) => s.changes.filter((c) => c.kind === kind).length;
  const part = (n: number, one: string) => (n ? `${n} ${one}${n === 1 ? "" : "s"}` : null);
  const summary = [part(count("config"), "Setting"), part(count("variable"), "Variable"), part(count("domain"), "Domain")]
    .filter(Boolean)
    .join(" · ");
  return { pendingChanges: s.changes.filter((c) => c.kind !== "service").length, isNew: true, newSummary: summary };
}
