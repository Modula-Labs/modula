import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Activity, BrandDocker, BrandGithub, Bucket, Copy, Database, Disk, LayoutGrid, Plus, Power, Refresh, Rocket, Trash } from "@/components/ui/icons";
import { ContextMenu, MenuItem, MenuLabel, MenuSeparator } from "@/components/ui/Menu";
import { useLingering } from "@/components/ui/Sheet";
import { api, type DeploymentStatus } from "@/lib/api";
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

export interface ProjectSearch {
  env?: string;
  service?: string;
  tab?: ServiceTab;
  /** Logs page: preselected time window, epoch milliseconds. */
  since?: number;
  until?: number;
  deployment?: string;
  /** Reopen the New Service dialog on this step (back from "Connect GitHub"). */
  add?: "repo";
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
  const [placeAt, setPlaceAt] = useState<{ x: number; y: number } | null>(null);
  const [menu, setMenu] = useState<{ at: CanvasPoint; serviceId?: string } | null>(null);
  const [deletingService, setDeletingService] = useState<{ id: string; name: string }>();
  const [notice, setNotice] = useState<string | null>(null);
  const [syncing, setSyncing] = useState(false);
  // Open by default; the choice is remembered in this browser.
  const [activityOpen, setActivityOpenState] = useState(() => load("mc.activity.open") !== "0");
  const setActivityOpen = (open: boolean | ((o: boolean) => boolean)) =>
    setActivityOpenState((prev) => {
      const next = typeof open === "function" ? open(prev) : open;
      save("mc.activity.open", next ? "1" : "0");
      return next;
    });
  const { project: p, env, role } = useProject();

  const pending = usePending(env?.id).data;
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
        stack: templateStack(s.template ?? i?.template) ?? i?.stack,
        volume: i?.volume?.mountPath ?? null,
        image: i?.sourceType === "image" ? i.image : undefined,
        dependsOn: i?.dependsOn,
        pendingChanges: pending?.services.find((x) => x.serviceId === s.id)?.changes.length ?? 0,
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
      openAdd("repo");
    } else if (back.notice) setNotice(back.notice.text);
    setSearch({ ...search, add: undefined, github: undefined });
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
            <Button
              variant="outline"
              icon={<Activity />}
              aria-pressed={activityOpen}
              onClick={() => setActivityOpen((o) => !o)}
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
                  const canRestart = !!inst?.latestDeploymentId && ["active", "crashed"].includes(inst.latestDeploymentStatus ?? "");
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
        {p && env && activityOpen && !instanceId ? (
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
        {p && services.length === 0 ? (
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
            <div className="pointer-events-auto flex flex-col items-center gap-3 rounded-xl border border-dashed border-border bg-card px-10 py-8 text-center">
              <div className="text-base font-bold">Add your first service</div>
              <p className="max-w-xs text-sm text-muted-foreground">Deploy from a GitHub repository or a Docker image, or add a database or a bucket.</p>
              {atLeast(role, "member") ? (
                <Button variant="outline" icon={<Plus />} onClick={() => openAdd("root")}>
                  Add a service
                </Button>
              ) : null}
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

      {p && env ? (
        <NewServiceDialog
          open={adding}
          onOpenChange={(o) => {
            setAdding(o);
            if (!o) {
              setPlaceAt(null);
              setGithubNotice(null);
            }
          }}
          initialStep={addStep}
          githubNotice={githubNotice}
          projectId={projectId}
          environmentId={env.id}
          onCreated={(service) => setSearch({ env: search.env, service, tab: "deployments" })}
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
