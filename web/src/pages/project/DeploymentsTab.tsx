import { useQuery } from "@tanstack/react-query";
import { clsx } from "clsx";
import { Rocket } from "@/components/ui/icons";
import { Badge, EmptyState, Skeleton } from "@/components/ui/misc";
import { api, type Deployment } from "@/lib/api";
import { relativeTime } from "@/lib/format";
import { deploymentBadge, isLive } from "@/lib/status";

/** Newest first. The serving deployment (active/crashed) gets its own card. */
export function DeploymentsTab({
  instanceId,
  selected,
  onOpen,
}: {
  instanceId: string;
  selected?: string;
  onOpen: (deploymentId: string) => void;
}) {
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
  return (
    <div className="flex flex-col gap-6">
      {serving ? <Row d={serving} selected={selected === serving.id} onOpen={onOpen} prominent /> : null}
      {rest.length ? (
        <section className="flex flex-col gap-2">
          <h3 className="text-caps text-muted-foreground">History</h3>
          <div className="divide-y divide-border rounded-lg border border-border">
            {rest.map((d) => (
              <Row key={d.id} d={d} selected={selected === d.id} onOpen={onOpen} />
            ))}
          </div>
        </section>
      ) : null}
    </div>
  );
}

function Row({
  d,
  selected,
  prominent,
  onOpen,
}: {
  d: Deployment;
  selected: boolean;
  prominent?: boolean;
  onOpen: (id: string) => void;
}) {
  const badge = deploymentBadge[d.status];
  return (
    <button
      onClick={() => onOpen(d.id)}
      className={clsx(
        "flex w-full items-center gap-4 px-4 text-left hover:bg-accent/60",
        prominent ? "min-h-20 rounded-lg border bg-card" : "min-h-14",
        prominent && (selected ? "border-foreground/50" : "border-border"),
        !prominent && selected && "bg-accent/60",
      )}
    >
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
  );
}
