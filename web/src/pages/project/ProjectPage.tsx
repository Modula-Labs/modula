import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { clsx } from "clsx";
import { useMemo, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Activity, Check, ChevronDown, LayoutGrid, Plus, Refresh, Server, Settings, Trash, Variable } from "@/components/ui/icons";
import { Field, Input, Select } from "@/components/ui/Input";
import { LogoMark } from "@/components/ui/Logo";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/Menu";
import { Modal } from "@/components/ui/Modal";
import { useLingering } from "@/components/ui/Sheet";
import { Alert, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type DeploymentStatus, type Environment } from "@/lib/api";
import { atLeast } from "@/lib/roles";
import { isLive } from "@/lib/status";
import { templateStack } from "@/lib/templates";
import { useMe } from "@/lib/workspace";
import { ActivityPanel } from "./ActivityPanel";
import { Canvas, type CanvasService } from "./Canvas";
import { DeploymentSheet } from "./DeploymentSheet";
import { NewServiceDialog } from "./NewServiceDialog";
import { PendingBar, usePending } from "./PendingBar";
import { ProjectSettingsDialog } from "./ProjectSettingsDialog";
import { SharedVariablesSheet } from "./SharedVariablesSheet";
import { SyncDialog } from "./SyncDialog";
import { ServiceSheet, type ServiceTab } from "./ServiceSheet";

export interface ProjectSearch {
  env?: string;
  service?: string;
  tab?: ServiceTab;
  deployment?: string;
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
  const { data: me } = useMe();
  const [adding, setAdding] = useState(false);
  const [newEnv, setNewEnv] = useState(false);
  const [settings, setSettings] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [activityOpen, setActivityOpen] = useState(false);
  const [sharedOpen, setSharedOpen] = useState(false);
  const [deletingEnv, setDeletingEnv] = useState<Environment>();
  const project = useQuery({
    queryKey: ["project", projectId],
    queryFn: () => api.project(projectId),
    refetchInterval: (q) =>
      q.state.data?.instances.some((i) => i.latestDeploymentStatus && isLive(i.latestDeploymentStatus)) ? 2000 : 10_000,
  });
  const p = project.data;
  const role = me?.workspaces.find((w) => w.id === p?.workspaceId)?.role;
  const env = p?.environments.find((e) => e.id === search.env) ?? p?.environments[0];

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
      };
    });
  }, [p, env]);
  const currentInstanceId = p?.instances.find((i) => i.serviceId === search.service && i.environmentId === env?.id)?.id;
  // Keep the sheets mounted while they slide out.
  const { value: instanceId, leaving: sheetLeaving } = useLingering(currentInstanceId);
  const { value: deploymentId, leaving: deploymentLeaving } = useLingering(currentInstanceId ? search.deployment : undefined);
  const canEdit = atLeast(role, "member");
  const canDeploy = atLeast(role, "deployer");
  const { value: sharedMounted, leaving: sharedLeaving } = useLingering(sharedOpen ? true : undefined);
  // While the "Deploy N changes" bar is up it owns the screen's primary button.
  const hasPending = (usePending(canDeploy ? env?.id : undefined).data?.count ?? 0) > 0;

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
    <div className="grid h-full grid-cols-[56px_1fr] grid-rows-[52px_1fr]">
      <Link to="/projects" aria-label="All projects" className="flex items-center justify-center text-foreground">
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
              <MenuItem key={e.id} icon={e.id === env.id ? <Check /> : <span className="size-4" />} onSelect={() => setSearch({ env: e.id })}>
                {e.name}
              </MenuItem>
            ))}
            <MenuSeparator />
            <MenuItem
              icon={<Variable />}
              onSelect={() => {
                setSearch({ env: search.env });
                setSharedOpen(true);
              }}
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
        {p ? (
          <button
            aria-label="Project settings"
            title="Project settings"
            onClick={() => setSettings(true)}
            className="flex size-8 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground [&>svg]:size-4"
          >
            <Settings />
          </button>
        ) : null}
      </header>

      <nav aria-label="Project" className="flex flex-col items-center gap-1 pt-2">
        <RailLink to="/projects" label="Projects" icon={<LayoutGrid />} />
        {me?.user.isInstanceAdmin ? <RailLink to="/servers" label="Servers" icon={<Server />} /> : null}
      </nav>

      <main className="relative mr-2 mb-2 min-h-0 overflow-hidden rounded-xl border border-border">
        {p && env ? (
          <Canvas
            projectId={projectId}
            services={services}
            selected={search.service}
            onSelect={(id) => {
              if (id) setSharedOpen(false);
              setSearch({ env: search.env, service: id, tab: id ? search.tab : undefined });
            }}
          />
        ) : (
          <div className="dotgrid h-full" />
        )}
        {p && env ? (
          <div className="absolute top-3 right-3 z-10 flex items-center gap-2">
            <Button
              variant="secondary"
              size="sm"
              icon={<Activity />}
              aria-pressed={activityOpen}
              onClick={() => setActivityOpen((o) => !o)}
            >
              Activity
            </Button>
            {canEdit ? (
              <>
                <Button
                  variant="secondary"
                  size="sm"
                  icon={<Refresh />}
                  disabled={p.environments.length < 2}
                  title={p.environments.length < 2 ? "Create another environment to sync from" : undefined}
                  onClick={() => setSyncing(true)}
                >
                  Sync
                </Button>
                <Button variant={hasPending ? "secondary" : "primary"} size="sm" icon={<Plus />} onClick={() => setAdding(true)}>
                  Add
                </Button>
              </>
            ) : null}
          </div>
        ) : null}
        {p && env && canDeploy ? <PendingBar projectId={projectId} environmentId={env.id} /> : null}
        {p && env && activityOpen ? (
          <ActivityPanel
            projectId={projectId}
            environmentId={env.id}
            services={p.services}
            onClose={() => setActivityOpen(false)}
            onOpenDeployment={(service, deployment) => {
              setActivityOpen(false);
              if (service) setSearch({ env: search.env, service, tab: "deployments", deployment });
            }}
          />
        ) : null}
        {p && services.length === 0 ? (
          <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
            <div className="pointer-events-auto flex flex-col items-center gap-3 rounded-xl border border-dashed border-border bg-card px-10 py-8 text-center">
              <div className="text-base font-semibold">Add your first service</div>
              <p className="max-w-xs text-sm text-muted-foreground">Deploy from a GitHub repository or a Docker image, or add a database.</p>
              {atLeast(role, "member") ? (
                <Button variant="outline" icon={<Plus />} onClick={() => setAdding(true)}>
                  Add a service
                </Button>
              ) : null}
            </div>
          </div>
        ) : null}

        {sharedMounted && env ? (
          <SharedVariablesSheet
            key={env.id}
            environmentId={env.id}
            environmentName={env.name}
            canEdit={canDeploy}
            leaving={sharedLeaving}
            onClose={() => setSharedOpen(false)}
          />
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
      </main>

      {p && env ? (
        <NewServiceDialog
          open={adding}
          onOpenChange={setAdding}
          projectId={projectId}
          environmentId={env.id}
          onCreated={(service) => setSearch({ env: search.env, service, tab: "deployments" })}
        />
      ) : null}
      {p && env ? (
        <SyncDialog open={syncing} onOpenChange={setSyncing} projectId={projectId} target={env} environments={p.environments} />
      ) : null}
      {p && env ? (
        <ProjectSettingsDialog
          open={settings}
          onOpenChange={setSettings}
          project={p}
          role={role}
          environmentId={env.id}
          onEnvironmentDeleted={() => setSearch({})}
        />
      ) : null}
      {p ? (
        <NewEnvironmentDialog
          open={newEnv}
          onOpenChange={setNewEnv}
          projectId={projectId}
          environments={p.environments}
          current={env}
          onCreated={(id) => setSearch({ env: id })}
        />
      ) : null}
      <DeleteEnvironmentDialog
        environment={deletingEnv}
        onClose={() => setDeletingEnv(undefined)}
        projectId={projectId}
        onDeleted={() => setSearch({})}
      />
    </div>
  );
}

function RailLink({ to, label, icon }: { to: string; label: string; icon: ReactNode }) {
  return (
    <Link
      to={to}
      title={label}
      aria-label={label}
      className={clsx("flex size-9 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground [&>svg]:size-[18px]")}
    >
      {icon}
    </Link>
  );
}

function NewEnvironmentDialog({
  open,
  onOpenChange,
  projectId,
  environments,
  current,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  projectId: string;
  environments: Environment[];
  /** Offered first when duplicating. */
  current?: Environment;
  onCreated: (id: string) => void;
}) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  // "" = empty: the server's default of the first environment's config, no variables.
  const [from, setFrom] = useState("");
  const [copyVariables, setCopyVariables] = useState(false);
  const create = useMutation({
    mutationFn: () =>
      api.createEnvironment(projectId, {
        name: name.trim(),
        ...(from ? { cloneFrom: from, copyVariables } : {}),
      }),
    onSuccess: async (e) => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onCreated(e.id);
      setName("");
      setFrom("");
      setCopyVariables(false);
      onOpenChange(false);
    },
  });
  const ordered = current ? [current, ...environments.filter((e) => e.id !== current.id)] : environments;
  return (
    <Modal open={open} onOpenChange={onOpenChange} title="New environment" description="Each environment gets its own copy of every service, with its own variables and domains.">
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          create.mutate();
        }}
      >
        {create.error ? <Alert>{create.error.message}</Alert> : null}
        <Field label="Name">
          {(p) => <Input {...p} autoFocus required maxLength={32} placeholder="staging" value={name} onChange={(e) => setName(e.target.value)} />}
        </Field>
        <Field label="Start from" hint={from ? undefined : "Services get the first environment's configuration, without variables."}>
          {(p) => (
            <Select {...p} value={from} onChange={(e) => setFrom(e.target.value)}>
              <option value="">Empty</option>
              {ordered.map((e) => (
                <option key={e.id} value={e.id}>
                  Duplicate {e.name}
                </option>
              ))}
            </Select>
          )}
        </Field>
        {from ? (
          <label className="flex items-center gap-2 text-sm text-muted-foreground">
            <input type="checkbox" className="size-4 accent-foreground" checked={copyVariables} onChange={(e) => setCopyVariables(e.target.checked)} />
            Copy variables (sealed ones are not copied)
          </label>
        ) : null}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending} disabled={!name.trim()}>
            Create environment
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** Deleting needs sudo; the API client opens the re-auth dialog and retries. */
function DeleteEnvironmentDialog({
  environment,
  onClose,
  projectId,
  onDeleted,
}: {
  environment?: Environment;
  onClose: () => void;
  projectId: string;
  onDeleted: () => void;
}) {
  const qc = useQueryClient();
  // Keep the name on screen while the dialog fades out.
  const [last, setLast] = useState(environment);
  if (environment && environment !== last) setLast(environment);
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteEnvironment(id),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onDeleted();
      onClose();
    },
  });
  return (
    <Modal
      open={!!environment}
      onOpenChange={(o) => {
        if (!o) {
          remove.reset();
          onClose();
        }
      }}
      title={`Delete ${last?.name ?? "environment"}?`}
      description="Removes its services, variables and deployments, and stops their containers. This can't be undone."
    >
      <div className="flex flex-col gap-4">
        {remove.error ? <Alert>{remove.error.message}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="danger" loading={remove.isPending} onClick={() => last && remove.mutate(last.id)}>
            Delete environment
          </Button>
        </div>
      </div>
    </Modal>
  );
}
