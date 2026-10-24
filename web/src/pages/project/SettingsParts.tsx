import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Select } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { api, type Instance, type NodeInfo } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";
import { AddServerDialog } from "@/pages/servers/AddServerDialog";
import { CPU_STEP, DEFAULT_CPU_MILLIS, DEFAULT_MEMORY_MB, MEM_STEP, capacity, formatCpu, formatMemory, fromSlider, serverLabel, stepsFor, toSlider } from "./resources";

export interface RailItem {
  id: string;
  label: string;
}

/** Railway-style quick links: stays put while the settings scroll, highlights the section in view. */
export function SectionRail({ items }: { items: RailItem[] }) {
  const [active, setActive] = useState(items[0]?.id);
  const key = items.map((i) => i.id).join();
  useEffect(() => {
    if (typeof IntersectionObserver === "undefined") return;
    const visible = new Set<string>();
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) e.isIntersecting ? visible.add(e.target.id) : visible.delete(e.target.id);
        const first = items.find((i) => visible.has(i.id));
        if (first) setActive(first.id);
      },
      { rootMargin: "0px 0px -60% 0px" },
    );
    for (const i of items) {
      const el = document.getElementById(i.id);
      if (el) io.observe(el);
    }
    return () => io.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  const jump = (id: string) => {
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    document.getElementById(id)?.scrollIntoView({ behavior: reduced ? "auto" : "smooth", block: "start" });
    setActive(id);
  };
  return (
    <nav aria-label="Settings sections" className="sticky top-0 hidden w-32 shrink-0 self-start pt-1 @[640px]:block">
      <ul className="flex flex-col gap-1.5 border-l border-border pl-3 text-sm">
        {items.map((i) => (
          <li key={i.id}>
            <button
              type="button"
              onClick={() => jump(i.id)}
              aria-current={active === i.id ? "location" : undefined}
              className={clsx(
                "text-left transition-colors hover:text-foreground",
                active === i.id ? "font-bold text-foreground" : "text-muted-foreground",
              )}
            >
              {i.label}
            </button>
          </li>
        ))}
      </ul>
    </nav>
  );
}

/** The instance's server, from the shared nodes query (undefined while loading or when not allowed). */
export function useNodes() {
  return useQuery({ queryKey: ["nodes"], queryFn: api.nodes, staleTime: 30_000, retry: false });
}

function LimitSlider({
  label,
  value,
  server,
  maxValue,
  step,
  format,
  fallback,
  disabled,
  onChange,
}: {
  label: string;
  value: string;
  server: string;
  maxValue: number;
  step: number;
  format: (v: string) => string;
  /** Where an unset limit sits: the server's default. */
  fallback: number;
  disabled: boolean;
  onChange: (v: string) => void;
}) {
  const steps = stepsFor(maxValue, step);
  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-baseline justify-between text-sm">
        <span className="text-muted-foreground">
          {label}: <span className="font-bold text-foreground">{format(value)}</span>
        </span>
        <span className="text-[13px] text-muted-foreground">Server: {server}</span>
      </div>
      <input
        type="range"
        aria-label={label}
        aria-valuetext={format(value)}
        min={1}
        max={steps}
        step={1}
        disabled={disabled}
        value={toSlider(value, maxValue, step, fallback)}
        onChange={(e) => onChange(fromSlider(Number(e.target.value), maxValue, step))}
        className="h-1.5 w-full cursor-pointer accent-foreground disabled:cursor-not-allowed disabled:opacity-50"
      />
    </div>
  );
}

/** CPU and memory limits as sliders; unset shows the default, all the way right is the whole server. */
export function ResourceSliders({
  node,
  cpuMillis,
  memoryMb,
  canEdit,
  onCpu,
  onMemory,
}: {
  node: NodeInfo | undefined;
  cpuMillis: string;
  memoryMb: string;
  canEdit: boolean;
  onCpu: (v: string) => void;
  onMemory: (v: string) => void;
}) {
  const cap = capacity(node);
  return (
    <div className="flex flex-col gap-5 rounded-xl border border-border p-4">
      <LimitSlider
        label="CPU"
        value={cpuMillis}
        server={formatCpu(cap.cpuMillis)}
        maxValue={cap.cpuMillis}
        step={CPU_STEP}
        format={(v) => formatCpu(v)}
        fallback={DEFAULT_CPU_MILLIS}
        disabled={!canEdit}
        onChange={onCpu}
      />
      <LimitSlider
        label="Memory"
        value={memoryMb}
        server={formatMemory(cap.memoryMb)}
        maxValue={cap.memoryMb}
        step={MEM_STEP}
        format={(v) => formatMemory(v)}
        fallback={DEFAULT_MEMORY_MB}
        disabled={!canEdit}
        onChange={onMemory}
      />
    </div>
  );
}

const NEW = "__new";

/** Which server the service runs on; picking another one moves it (redeploys there). */
export function ServerPicker({
  instance,
  nodes,
  workspaceId,
  canEdit,
}: {
  instance: Instance;
  nodes: NodeInfo[] | undefined;
  workspaceId: string | undefined;
  canEdit: boolean;
}) {
  const qc = useQueryClient();
  const [target, setTarget] = useState<NodeInfo | null>(null);
  const [adding, setAdding] = useState(false);
  const usable = (nodes ?? []).filter((n) => n.status !== "revoked");
  const current = usable.find((n) => n.id === instance.nodeId);
  const move = useMutation({
    mutationFn: (nodeId: string) => api.moveInstance(instance.id, nodeId),
    onSuccess: async () => {
      setTarget(null);
      await Promise.all([
        qc.invalidateQueries({ queryKey: ["project", instance.projectId] }),
        qc.invalidateQueries({ queryKey: ["instance", instance.id] }),
        qc.invalidateQueries({ queryKey: ["deployments", instance.id] }),
        qc.invalidateQueries({ queryKey: ["pending", instance.environmentId] }),
        qc.invalidateQueries({ queryKey: ["nodes"] }),
      ]);
    },
  });
  const onPick = (v: string) => {
    if (v === NEW) return setAdding(true);
    const n = usable.find((x) => x.id === v);
    if (n && n.id !== instance.nodeId) {
      move.reset();
      setTarget(n);
    }
  };
  const fallbackLabel = instance.location
    ? `${[flagEmoji(instance.location.countryCode), instance.location.label].filter(Boolean).join(" ")}`
    : "Server unknown";
  const city = (n: NodeInfo) => n.location?.label ?? "its server";
  return (
    <div className="flex flex-col gap-3">
      <Select
        aria-label="Server"
        disabled={!canEdit || !nodes}
        value={current?.id ?? ""}
        onChange={(e) => onPick(e.target.value)}
      >
        {current ? null : <option value="">{fallbackLabel}</option>}
        {usable.map((n) => (
          <option key={n.id} value={n.id}>
            {serverLabel(n)}
          </option>
        ))}
        <option value={NEW}>+ New server…</option>
      </Select>
      {move.error && !target ? <Alert>{move.error.message}</Alert> : null}
      {target ? (
        <div className="flex flex-col gap-3 rounded-xl border border-border p-4 text-sm">
          <p className="text-muted-foreground">
            Move <span className="text-foreground">{instance.serviceName}</span> to{" "}
            <span className="text-foreground">{target.name}</span> in {city(target)}? It redeploys there; the old one
            stops once the new one is live.
          </p>
          {move.error ? <Alert>{move.error.message}</Alert> : null}
          <div className="flex gap-2">
            <Button size="sm" variant="secondary" loading={move.isPending} onClick={() => move.mutate(target.id)}>
              Move service
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setTarget(null)}>
              Cancel
            </Button>
          </div>
        </div>
      ) : null}
      <AddServerDialog
        open={adding}
        onOpenChange={(o) => {
          setAdding(o);
          if (!o) void qc.invalidateQueries({ queryKey: ["nodes"] });
        }}
        workspaceId={workspaceId}
      />
    </div>
  );
}
