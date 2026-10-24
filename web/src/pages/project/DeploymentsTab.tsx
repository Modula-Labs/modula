import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { ChevronDown, DotsVertical, FileText, IconMapPin, IconRollback, IconStack2, Refresh, Rocket, Trash, World } from "@/components/ui/icons";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/Menu";
import { Alert, Badge, Skeleton, StatusDot } from "@/components/ui/misc";
import { SourceAvatar } from "@/components/ui/SourceAvatar";
import { api, type Deployment, type DeploymentDetail, type Instance, type InstanceState } from "@/lib/api";
import { deploymentTitle, sleepingLine, viaLabel } from "@/lib/deployments";
import { flagEmoji } from "@/lib/flags";
import { relativeTime } from "@/lib/format";
import { badgeOf, isLive, isServingStatus } from "@/lib/status";
import { load, save } from "@/lib/storage";

const HIDE_SKIPPED_KEY = "modula.hideSkipped";
const HISTORY_OPEN_KEY = "modula.historyOpen";

type Action = "redeploy" | "rollback" | "restart" | "remove";

interface RowActions {
  canDeploy: boolean;
  /** The serving deployment's id, which rollback must not target. */
  servingId: string | undefined;
  run: (action: Action, id: string) => void;
}

const isServing = (d: Deployment) => isServingStatus(d.status);

/** Newest first. The serving deployment (active/sleeping/crashed) gets its own card. */
export function DeploymentsTab({
  instanceId,
  selected,
  canDeploy,
  onOpen,
}: {
  instanceId: string;
  selected?: string;
  canDeploy: boolean;
  onOpen: (deploymentId: string) => void;
}) {
  const qc = useQueryClient();
  const [hideSkipped, setHideSkipped] = useState(() => load(HIDE_SKIPPED_KEY) === "1");
  const [historyOpen, setHistoryOpen] = useState(() => load(HISTORY_OPEN_KEY) !== "0");
  const list = useQuery({
    queryKey: ["deployments", instanceId],
    queryFn: () => api.deployments(instanceId),
    refetchInterval: (q) => (q.state.data?.some((d) => isLive(d.status)) ? 2000 : 15_000),
  });
  const instance = useQuery({ queryKey: ["instance", instanceId], queryFn: () => api.serviceInstance(instanceId) });

  const refresh = (dep?: DeploymentDetail) => {
    void qc.invalidateQueries({ queryKey: ["deployments", instanceId] });
    void qc.invalidateQueries({ queryKey: ["project"] });
    void qc.invalidateQueries({ queryKey: ["instance", instanceId] });
    if (dep) onOpen(dep.id);
  };
  const act = useMutation({
    mutationFn: ({ action, id }: { action: Action; id: string }) =>
      action === "redeploy"
        ? api.redeployDeployment(id)
        : action === "rollback"
          ? api.rollbackDeployment(id)
          : action === "restart"
            ? api.restartDeployment(id)
            : api.removeDeployment(id),
    onSuccess: (dep, { action }) => refresh(action === "remove" ? undefined : dep),
  });
  const deploy = useMutation({ mutationFn: () => api.deploy(instanceId), onSuccess: refresh });

  if (list.isPending) return <Skeleton className="h-40" />;
  const all = list.data ?? [];
  const serving = all.find(isServing);
  const rest = all.filter((d) => d !== serving);
  const history = hideSkipped ? rest.filter((d) => d.status !== "cancelled") : rest;
  const actions: RowActions = { canDeploy, servingId: serving?.id, run: (action, id) => act.mutate({ action, id }) };
  const error = act.error ?? deploy.error ?? list.error;
  const toggleSkipped = () => {
    save(HIDE_SKIPPED_KEY, hideSkipped ? "0" : "1");
    setHideSkipped(!hideSkipped);
  };
  const toggleHistory = () => {
    save(HISTORY_OPEN_KEY, historyOpen ? "0" : "1");
    setHistoryOpen(!historyOpen);
  };

  return (
    <div className="flex flex-col gap-6">
      {error ? <Alert>{error.message}</Alert> : null}
      <StatusBar instance={instance.data} loading={instance.isPending} serving={serving} latest={all[0]} />
      {serving ? (
        <DeploymentRow d={serving} selected={selected === serving.id} onOpen={onOpen} actions={actions} prominent />
      ) : (
        <EmptyActive instance={instance.data} canDeploy={canDeploy} loading={deploy.isPending} onDeploy={() => deploy.mutate()} />
      )}
      {rest.length ? (
        <section className="flex flex-col gap-2">
          <div className="flex items-center justify-between gap-3">
            <button
              onClick={toggleHistory}
              aria-expanded={historyOpen}
              className="flex items-center gap-1 rounded-md py-1 text-sm font-bold text-foreground"
            >
              <ChevronDown className={clsx("size-3.5 transition-transform duration-150", !historyOpen && "-rotate-90")} />
              History
            </button>
            <button
              onClick={toggleSkipped}
              className={clsx("text-sm text-muted-foreground transition-opacity duration-150 hover:text-foreground", !historyOpen && "pointer-events-none opacity-0")}
              tabIndex={historyOpen ? 0 : -1}
            >
              {hideSkipped ? "Show skipped" : "Hide skipped"}
            </button>
          </div>
          <div
            className={clsx(
              "grid transition-[grid-template-rows,opacity] duration-200 ease-out",
              historyOpen ? "grid-rows-[1fr] opacity-100" : "grid-rows-[0fr] opacity-0",
            )}
            inert={!historyOpen}
          >
            <div className="min-h-0 overflow-hidden">
              {history.length ? (
                <div className="divide-y divide-border overflow-hidden rounded-xl border border-border">
                  {history.map((d) => (
                    <DeploymentRow key={d.id} d={d} selected={selected === d.id} onOpen={onOpen} actions={actions} />
                  ))}
                </div>
              ) : (
                <p className="rounded-xl border border-dashed border-border px-4 py-6 text-center text-sm text-muted-foreground">
                  Only skipped deployments so far.
                </p>
              )}
            </div>
          </div>
        </section>
      ) : null}
    </div>
  );
}

/** What the service is doing now, from the instance when the server reports it. */
function stateOf(instance: Instance | undefined, serving: Deployment | undefined, latest: Deployment | undefined): InstanceState {
  if (instance?.status) return instance.status;
  if (serving)
    return serving.status === "crashed"
      ? "crashed"
      : serving.status === "sleeping"
        ? "sleeping"
        : serving.degraded
          ? "degraded"
          : "online";
  return latest && isLive(latest.status) ? "deploying" : "offline";
}

const stateView: Record<InstanceState, { label: string; tone: "success" | "warn" | "danger" | "neutral" }> = {
  online: { label: "Online", tone: "success" },
  sleeping: { label: "Sleeping", tone: "neutral" },
  degraded: { label: "Degraded: a replica is down", tone: "warn" },
  offline: { label: "Service offline", tone: "neutral" },
  deploying: { label: "Deploying…", tone: "warn" },
  crashed: { label: "Crashed", tone: "danger" },
};

function StatusBar({
  instance,
  loading,
  serving,
  latest,
}: {
  instance: Instance | undefined;
  loading: boolean;
  serving: Deployment | undefined;
  latest: Deployment | undefined;
}) {
  if (loading) return <Skeleton className="h-5 w-64" />;
  const state = stateView[stateOf(instance, serving, latest)];
  const location = instance?.location;
  const replicas = instance?.config.replicas ?? 1;
  return (
    <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 text-sm text-muted-foreground">
      <span className="inline-flex items-center gap-2 [&_svg]:size-4">
        <World />
        <StatusDot tone={state.tone}>{state.label}</StatusDot>
      </span>
      <span className="inline-flex items-center gap-4">
        {location ? (
          <span className="inline-flex items-center gap-1.5 [&_svg]:size-4" title={location.provider}>
            <IconMapPin />
            {location.label}
            {flagEmoji(location.countryCode) ? <span aria-hidden>{flagEmoji(location.countryCode)}</span> : null}
          </span>
        ) : null}
        <span className="inline-flex items-center gap-1.5 [&_svg]:size-4">
          <IconStack2 />
          {replicas} {replicas === 1 ? "Replica" : "Replicas"}
        </span>
      </span>
    </div>
  );
}

function EmptyActive({
  instance,
  canDeploy,
  loading,
  onDeploy,
}: {
  instance: Instance | undefined;
  canDeploy: boolean;
  loading: boolean;
  onDeploy: () => void;
}) {
  const source = instance?.config.source;
  const target =
    source?.type === "repo" ? source.repo : source?.type === "image" ? source.image : null;
  return (
    <div className="flex min-h-20 flex-col items-center justify-center gap-1 rounded-xl border border-dashed border-border px-4 py-5 text-center text-sm text-muted-foreground">
      <p>There is no active deployment for this service.</p>
      {source?.type === "local" ? (
        <p>
          Deploy it from its directory with <code className="font-mono text-foreground">modula up</code>.
        </p>
      ) : null}
      {canDeploy && target ? (
        <button
          onClick={onDeploy}
          disabled={loading}
          className="max-w-full truncate rounded-sm text-foreground underline-offset-4 hover:underline disabled:opacity-60"
        >
          {loading ? "Deploying…" : source?.type === "repo" ? "Deploy the repo " : "Deploy "}
          {loading ? null : <strong className="font-bold">{target}</strong>}
        </button>
      ) : null}
    </div>
  );
}

function DeploymentRow({
  d,
  selected,
  prominent,
  onOpen,
  actions,
}: {
  d: Deployment;
  selected: boolean;
  prominent?: boolean;
  onOpen: (id: string) => void;
  actions: RowActions;
}) {
  const badge = badgeOf(d);
  const serving = isServing(d);
  const live = isLive(d.status);
  const title = deploymentTitle(d);
  const imageOnly = !d.commitMessage && !d.commitSha && d.sourceKind !== "upload";
  const rebuildable = (d.sourceKind === "github" && !!d.commitSha) || (d.sourceKind === "upload" && !!d.uploadAvailable);
  const canRollback = rebuildable && d.id !== actions.servingId && d.status !== "queued" && !live;
  const canRedeploy = (d.sourceKind === "github" || d.sourceKind === "image" || !d.sourceKind || rebuildable) && !live;
  return (
    <div
      className={clsx(
        "group relative flex items-center transition-colors duration-150 hover:bg-accent/60",
        prominent ? "min-h-20 rounded-xl border bg-card" : "min-h-16",
        prominent && (selected ? "border-foreground/50" : "border-border"),
        !prominent && selected && "bg-accent/60",
      )}
    >
      <button
        onClick={() => onOpen(d.id)}
        className="flex min-w-0 flex-1 items-center gap-3 self-stretch py-3 pl-4 text-left"
        aria-label={`Open deployment: ${title}`}
      >
        <span className="w-20 shrink-0">
          <Badge tone={badge.tone}>{badge.label}</Badge>
        </span>
        <SourceAvatar d={d} size={prominent ? 40 : 32} />
        <span className="min-w-0 flex-1">
          <span className={clsx("block truncate text-sm", imageOnly && "font-mono")}>{title}</span>
          <span className="block truncate text-[13px] text-muted-foreground">
            {relativeTime(d.createdAt)} {viaLabel(d)}
            {d.hosting === "cloudflare" ? " · Cloudflare" : ""}
          </span>
          {d.status === "sleeping" ? <span className="block truncate text-[13px] text-muted-foreground">{sleepingLine(d)}</span> : null}
          {d.error ? <span className="block truncate text-[13px] text-error">{d.error}</span> : null}
        </span>
      </button>
      <div className="flex shrink-0 items-center gap-1 px-2">
        {d.hosting === "cloudflare" && d.url ? (
          <a href={d.url} target="_blank" rel="noreferrer" className="max-w-48 truncate text-xs text-muted-foreground hover:text-foreground hover:underline max-sm:hidden">
            {d.url.replace(/^https?:\/\//, "")}
          </a>
        ) : null}
        <span
          className={clsx(
            "transition-opacity duration-150",
            !prominent && "opacity-0 group-focus-within:opacity-100 group-hover:opacity-100 max-sm:hidden",
          )}
        >
          <Button size="sm" variant="outline" icon={<FileText />} onClick={() => onOpen(d.id)}>
            View logs
          </Button>
        </span>
        <Menu
          trigger={
            <Button aria-label="Deployment actions" variant="ghost" size="icon-sm" className="text-muted-foreground">
              <DotsVertical />
            </Button>
          }
        >
          <MenuItem icon={<FileText />} onSelect={() => onOpen(d.id)}>
            View logs
          </MenuItem>
          {actions.canDeploy && canRedeploy ? (
            <MenuItem icon={<Rocket />} onSelect={() => actions.run("redeploy", d.id)} title="Rebuild this commit or repull this image with the current settings and variables">
              Redeploy
            </MenuItem>
          ) : null}
          {actions.canDeploy && canRollback ? (
            <MenuItem icon={<IconRollback />} onSelect={() => actions.run("rollback", d.id)} title="Rebuild this commit with its original settings and variables">
              Rollback
            </MenuItem>
          ) : null}
          {actions.canDeploy && serving && d.hosting !== "cloudflare" ? (
            <MenuItem icon={<Refresh />} onSelect={() => actions.run("restart", d.id)}>
              Restart
            </MenuItem>
          ) : null}
          {actions.canDeploy && (serving || live) ? (
            <>
              <MenuSeparator />
              <MenuItem danger icon={<Trash />} onSelect={() => actions.run("remove", d.id)}>
                {serving ? "Remove" : "Abort"}
              </MenuItem>
            </>
          ) : null}
        </Menu>
      </div>
    </div>
  );
}
