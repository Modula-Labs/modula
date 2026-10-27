import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { ArrowLeft, DotsVertical, Server, Trash } from "@/components/ui/icons";
import { Menu, MenuItem } from "@/components/ui/Menu";
import { Alert, Badge, EmptyState, Skeleton, StatusDot } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import { api, type MetricsRange, type NodeInfo, type NodeMetrics } from "@/lib/api";
import { formatBytes, relativeTime } from "@/lib/format";
import { METRICS_RANGES, formatRate, niceMax } from "@/lib/metrics";
import { MetricsChart } from "../project/MetricsChart";
import { MigrateDialog } from "./MigrateDialog";
import { ResizeDialog } from "./ResizeDialog";
import { RANGE_SECS } from "../project/MetricsPanels";
import { NodeLocation, gb, nodeStatus } from "../Servers";

/** One server: what it is, where it runs, and its own CPU, memory, network and disk over time. */
export function ServerDetailPage({ nodeId }: { nodeId: string }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const nodes = useQuery({ queryKey: ["nodes"], queryFn: api.nodes, refetchInterval: 10_000 });
  const n = nodes.data?.find((x) => x.id === nodeId);
  const revoke = useMutation({
    mutationFn: () => api.revokeNode(nodeId),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["nodes"] });
      void navigate({ to: "/servers" });
    },
  });

  return (
    <div>
      <Link
        to="/servers"
        className="inline-flex items-center gap-1.5 text-sm text-muted-foreground hover:text-foreground [&>svg]:size-4"
      >
        <ArrowLeft />
        Servers
      </Link>
      {nodes.isPending ? (
        <Skeleton className="mt-4 h-24" />
      ) : nodes.error ? (
        <div className="mt-4">
          <Alert>{nodes.error.message}</Alert>
        </div>
      ) : !n ? (
        <div className="mt-4">
          <EmptyState icon={<Server />} title="Server not found" body="It may have been revoked." />
        </div>
      ) : (
        <>
          <Header n={n} onRevoke={() => revoke.mutate()} />
          {revoke.error ? (
            <div className="mt-4">
              <Alert>{revoke.error.message}</Alert>
            </div>
          ) : null}
          <Usage n={n} />
        </>
      )}
    </div>
  );
}

function Header({ n, onRevoke }: { n: NodeInfo; onRevoke: () => void }) {
  const { tone, label } = nodeStatus(n);
  const [resizing, setResizing] = useState(false);
  const [migrating, setMigrating] = useState(false);
  const facts = [
    n.info.os,
    n.info.arch,
    n.info.cpus ? `${n.info.cpus} vCPU` : null,
    n.info.memoryBytes ? `${gb(n.info.memoryBytes)} RAM` : null,
    n.nodeVersion ? `modula-node v${n.nodeVersion}` : null,
    n.runtime,
  ].filter(Boolean);
  return (
    <div className="mt-3">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-3">
            <h1 className="truncate text-2xl leading-8 font-bold">{n.name}</h1>
            <StatusDot tone={tone}>{label}</StatusDot>
            {n.connected && n.appliedGeneration < n.desiredGeneration ? <Badge tone="warn">Syncing</Badge> : null}
          </div>
          <div className="mt-1 flex flex-wrap items-center gap-x-4 gap-y-1 text-sm text-muted-foreground">
            <NodeLocation n={n} />
            {n.publicIp ? (
              <span className="flex items-center gap-0.5">
                <span className="font-mono text-foreground">{n.publicIp}</span>
                <CopyButton value={n.publicIp} label="Copy public IP" />
              </span>
            ) : null}
            <span>{n.lastSeenAt ? `Seen ${relativeTime(n.lastSeenAt)}` : "Never connected"}</span>
          </div>
          <div className="mt-1 text-[13px] text-muted-foreground">{facts.join(" · ")}</div>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {n.status !== "revoked" ? (
            <>
              <Button variant="secondary" onClick={() => setResizing(true)}>
                Resize
              </Button>
              <Button variant="secondary" onClick={() => setMigrating(true)}>
                Migrate
              </Button>
            </>
          ) : null}
          {n.status !== "revoked" ? (
            <Menu
              trigger={
                <Button variant="ghost" size="icon" aria-label={`Actions for ${n.name}`}>
                  <DotsVertical />
                </Button>
              }
            >
              <MenuItem danger icon={<Trash />} onSelect={onRevoke}>
                Revoke server
              </MenuItem>
            </Menu>
          ) : null}
        </div>
      </div>
      <ResizeDialog
        open={resizing}
        onOpenChange={setResizing}
        nodeId={n.id}
        node={n}
        onMigrate={() => {
          setResizing(false);
          setMigrating(true);
        }}
      />
      <MigrateDialog open={migrating} onOpenChange={setMigrating} nodeId={n.id} />
    </div>
  );
}

function Usage({ n }: { n: NodeInfo }) {
  const [range, setRange] = useState<MetricsRange>("1h");
  const q = useQuery({
    queryKey: ["node-metrics", n.id, range],
    queryFn: () => api.nodeMetrics(n.id, range),
    refetchInterval: range === "1h" || range === "6h" ? 30_000 : false,
  });
  const m = q.data;
  return (
    <div className="mt-6 flex flex-col gap-4">
      <div className="flex items-center justify-between gap-4">
        <h2 className="text-base leading-6 font-bold">Usage</h2>
        <Segmented label="Time range" value={range} onChange={setRange} options={METRICS_RANGES.map((r) => ({ value: r, label: r }))} />
      </div>
      {q.error ? <Alert>{q.error.message}</Alert> : null}
      {q.isLoading ? (
        <div className="grid gap-4 lg:grid-cols-2">
          <Skeleton className="h-52" />
          <Skeleton className="h-52" />
          <Skeleton className="h-52" />
          <Skeleton className="h-52" />
        </div>
      ) : m && m.points.length === 0 && m.disk.length === 0 ? (
        <div className="rounded-xl border border-dashed border-border px-6 py-12 text-center text-sm text-muted-foreground">
          No usage yet — it appears a minute after the server connects.
        </div>
      ) : m ? (
        <Charts m={m} range={range} cpus={n.info.cpus ?? 0} />
      ) : null}
    </div>
  );
}

const percent = (v: number) => `${Math.round(v)}%`;
const load = (v: number) => (Math.round(v * 100) / 100).toString();
const DISK_COLORS = ["var(--chart-1)", "var(--chart-4)", "var(--chart-2)", "var(--chart-3)"];

function Charts({ m, range, cpus }: { m: NodeMetrics; range: MetricsRange; cpus: number }) {
  const pts = m.points;
  const last = pts[pts.length - 1];
  const lastDisk = Math.max(0, ...m.disk.flatMap((d) => d.points.map((p) => p.t)));
  // The window ends at the newest sample (or now when idle) so axes don't jump around.
  const to = Math.max(last?.t ?? 0, lastDisk, Date.now() - m.stepSecs * 1000);
  const from = to - RANGE_SECS[range] * 1000;
  const common = { range, from, to, maxGapMs: m.stepSecs * 2000 };

  const cpu = pts.map((p) => ({ t: p.t, v: p.cpuPercent }));
  const mem = pts.map((p) => ({ t: p.t, v: p.memoryUsedBytes }));
  const memTotal = Math.max(0, ...pts.map((p) => p.memoryTotalBytes));
  const rx = pts.map((p) => ({ t: p.t, v: p.netRxBytesPerSec }));
  const tx = pts.map((p) => ({ t: p.t, v: p.netTxBytesPerSec }));
  const load1 = pts.map((p) => ({ t: p.t, v: p.load1 }));
  const loadMax = Math.max(0, ...load1.map((p) => p.v));
  const loadNearCpus = cpus > 0 && loadMax >= cpus * 0.7;

  const diskTotal = Math.max(0, ...m.disk.flatMap((d) => d.points.map((p) => p.totalBytes)));
  const diskLast = m.disk.map((d) => d.points[d.points.length - 1]).filter((p) => p !== undefined);

  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <MetricsChart
        {...common}
        title="CPU"
        latest={last ? percent(last.cpuPercent) : "—"}
        series={[{ name: "CPU", color: "var(--chart-1)", data: cpu }]}
        max={100}
        yLabel={percent}
        valueLabel={percent}
      />
      <MetricsChart
        {...common}
        title="Memory"
        latest={last ? `${formatBytes(last.memoryUsedBytes)} / ${formatBytes(last.memoryTotalBytes)}` : "—"}
        series={[{ name: "Memory", color: "var(--chart-1)", data: mem }]}
        max={niceMax(Math.max(memTotal, ...mem.map((p) => p.v), 1))}
        yLabel={formatBytes}
        valueLabel={formatBytes}
        reference={memTotal ? { value: memTotal, label: `Total ${formatBytes(memTotal)}` } : undefined}
      />
      <MetricsChart
        {...common}
        title="Network"
        latest={last ? `↓ ${formatRate(last.netRxBytesPerSec)}  ↑ ${formatRate(last.netTxBytesPerSec)}` : "—"}
        series={[
          { name: "In", color: "var(--chart-1)", data: rx },
          { name: "Out", color: "var(--chart-4)", data: tx },
        ]}
        max={niceMax(Math.max(...rx.map((p) => p.v), ...tx.map((p) => p.v), 1))}
        yLabel={formatRate}
        valueLabel={formatRate}
      />
      <MetricsChart
        {...common}
        title="Load"
        latest={last ? load(last.load1) : "—"}
        series={[{ name: "Load (1m)", color: "var(--chart-1)", data: load1 }]}
        max={niceMax(Math.max(1, loadMax, loadNearCpus ? cpus : 0))}
        yLabel={load}
        valueLabel={load}
        reference={loadNearCpus ? { value: cpus, label: `${cpus} vCPU` } : undefined}
      />
      <MetricsChart
        {...common}
        maxGapMs={m.diskStepSecs * 2000}
        title="Disk"
        latest={diskLast.map((p) => `${formatBytes(p.usedBytes)} / ${formatBytes(p.totalBytes)}`).join(" · ") || "—"}
        series={m.disk.map((d, i) => ({
          name: d.path,
          color: DISK_COLORS[i % DISK_COLORS.length]!,
          data: d.points.map((p) => ({ t: p.t, v: p.usedBytes })),
        }))}
        max={niceMax(Math.max(diskTotal, 1))}
        yLabel={formatBytes}
        valueLabel={formatBytes}
        reference={m.disk.length === 1 && diskTotal ? { value: diskTotal, label: `Total ${formatBytes(diskTotal)}` } : undefined}
      />
    </div>
  );
}
