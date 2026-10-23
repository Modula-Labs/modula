import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/Menu";
import { DotsVertical, FileText, Refresh, Rocket, Trash } from "@/components/ui/icons";
import { Alert, Badge, EmptyState, Skeleton } from "@/components/ui/misc";
import { api, type Deployment } from "@/lib/api";
import { relativeTime } from "@/lib/format";
import { deploymentBadge, isLive } from "@/lib/status";

/** Newest first. The serving deployment (active/crashed) gets its own card. */
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
  const redeploy = useMutation({
    mutationFn: () => api.deploy(instanceId),
    onSuccess: (dep) => {
      void qc.invalidateQueries({ queryKey: ["deployments", instanceId] });
      void qc.invalidateQueries({ queryKey: ["project"] });
      onOpen(dep.id);
    },
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.removeDeployment(id),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["deployments", instanceId] });
      void qc.invalidateQueries({ queryKey: ["project"] });
    },
  });
  const actions: RowActions = {
    canDeploy,
    onRedeploy: () => redeploy.mutate(),
    onRemove: (id) => remove.mutate(id),
  };
  const list = useQuery({
    queryKey: ["deployments", instanceId],
    queryFn: () => api.deployments(instanceId),
    refetchInterval: (q) => (q.state.data?.some((d) => isLive(d.status)) ? 2000 : 15_000),
  });
  if (list.isPending) return <Skeleton className="h-40" />;
  const all = list.data ?? [];
  if (all.length === 0) {
    return (
      <EmptyState
        icon={<Rocket />}
        title="No deployments yet"
        body="Deploy the service to start it on one of your servers."
      />
    );
  }
  const serving = all.find((d) => d.status === "active" || d.status === "crashed");
  const rest = all.filter((d) => d !== serving);
  const error = redeploy.error ?? remove.error;
  return (
    <div className="flex flex-col gap-6">
      {error ? <Alert>{error.message}</Alert> : null}
      {serving ? <Row d={serving} selected={selected === serving.id} onOpen={onOpen} actions={actions} prominent /> : null}
      {rest.length ? (
        <section className="flex flex-col gap-2">
          <h3 className="text-caps text-muted-foreground">History</h3>
          <div className="divide-y divide-border rounded-lg border border-border">
            {rest.map((d) => (
              <Row key={d.id} d={d} selected={selected === d.id} onOpen={onOpen} actions={actions} />
            ))}
          </div>
        </section>
      ) : null}
    </div>
  );
}

interface RowActions {
  canDeploy: boolean;
  onRedeploy: () => void;
  onRemove: (id: string) => void;
}

function Row({
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
  const badge = deploymentBadge[d.status];
  const serving = d.status === "active" || d.status === "crashed";
  const removable = serving || isLive(d.status);
  return (
    <div
      className={clsx(
        "flex items-center hover:bg-accent/60",
        prominent ? "min-h-20 rounded-lg border bg-card" : "min-h-14",
        prominent && (selected ? "border-foreground/50" : "border-border"),
        !prominent && selected && "bg-accent/60",
      )}
    >
      <button onClick={() => onOpen(d.id)} className="flex min-w-0 flex-1 items-center gap-4 self-stretch pl-4 text-left">
        <Badge tone={badge.tone}>{badge.label}</Badge>
        <div className="min-w-0 flex-1">
          <div className="truncate font-mono text-[13px]">{d.image}</div>
          {d.error ? <div className="truncate text-xs text-error">{d.error}</div> : null}
        </div>
        <div className="shrink-0 text-right text-xs text-muted-foreground">
          <div>{relativeTime(d.createdAt)}</div>
          <div className="capitalize">{d.trigger}</div>
        </div>
      </button>
      <div className="px-2" onClick={(e) => e.stopPropagation()}>
        <Menu
          trigger={
            <button
              aria-label="Deployment actions"
              className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground [&_svg]:size-4"
            >
              <DotsVertical />
            </button>
          }
        >
          <MenuItem icon={<FileText />} onSelect={() => onOpen(d.id)}>
            View logs
          </MenuItem>
          {actions.canDeploy && serving ? (
            <MenuItem icon={<Refresh />} onSelect={actions.onRedeploy}>
              Redeploy
            </MenuItem>
          ) : null}
          {actions.canDeploy && removable ? (
            <>
              <MenuSeparator />
              <MenuItem danger icon={<Trash />} onSelect={() => actions.onRemove(d.id)}>
                {serving ? "Remove" : "Cancel"}
              </MenuItem>
            </>
          ) : null}
        </Menu>
      </div>
    </div>
  );
}
