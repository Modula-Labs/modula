import {
  Background,
  BackgroundVariant,
  Handle,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useNodesState,
  useReactFlow,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/base.css";
import { clsx } from "clsx";
import { useEffect } from "react";
import { BrandDocker, Maximize, Minus, Plus } from "@/components/ui/icons";
import { StatusDot } from "@/components/ui/misc";
import type { DeploymentStatus } from "@/lib/api";
import { serviceState } from "@/lib/status";
import { load, save } from "@/lib/storage";

export interface CanvasService {
  id: string;
  name: string;
  domain?: string;
  status: DeploymentStatus | null;
}

type ServiceNode = Node<{ service: CanvasService; selected: boolean }, "service">;

function ServiceCard({ data }: NodeProps<ServiceNode>) {
  const { service, selected } = data;
  const state = serviceState(service.status);
  return (
    <div
      className={clsx(
        "flex h-36 w-72 cursor-pointer flex-col rounded-xl border bg-card p-5 transition-colors",
        selected ? "border-foreground/50" : "border-border hover:border-input",
      )}
    >
      <Handle type="target" position={Position.Left} className="!opacity-0" />
      <div className="flex items-center gap-2.5">
        <BrandDocker className="size-5 text-muted-foreground" />
        <span className="truncate text-base font-semibold">{service.name}</span>
      </div>
      <div className="mt-1 truncate text-sm text-muted-foreground">{service.domain ?? " "}</div>
      <div className="mt-auto">
        <StatusDot tone={state.tone}>{state.label}</StatusDot>
      </div>
      <Handle type="source" position={Position.Right} className="!opacity-0" />
    </div>
  );
}

const nodeTypes = { service: ServiceCard };

/** Card positions are kept per project in this browser until the server stores layouts. */
function savedPositions(projectId: string): Record<string, { x: number; y: number }> {
  try {
    return JSON.parse(load(`mc.canvas.${projectId}`) ?? "{}");
  } catch {
    return {};
  }
}

function defaultPosition(i: number) {
  return { x: (i % 3) * 340, y: Math.floor(i / 3) * 200 };
}

function CanvasInner({
  projectId,
  services,
  selected,
  onSelect,
}: {
  projectId: string;
  services: CanvasService[];
  selected?: string;
  onSelect: (id: string | undefined) => void;
}) {
  const [nodes, setNodes, onNodesChange] = useNodesState<ServiceNode>([]);
  const flow = useReactFlow();

  useEffect(() => {
    const saved = savedPositions(projectId);
    setNodes((prev) =>
      services.map((s, i) => ({
        id: s.id,
        type: "service" as const,
        position: prev.find((n) => n.id === s.id)?.position ?? saved[s.id] ?? defaultPosition(i),
        data: { service: s, selected: s.id === selected },
      })),
    );
  }, [projectId, services, selected, setNodes]);

  return (
    <ReactFlow
      nodes={nodes}
      edges={[]}
      nodeTypes={nodeTypes}
      onNodesChange={onNodesChange}
      onNodeClick={(_, n) => onSelect(n.id)}
      onPaneClick={() => onSelect(undefined)}
      onNodeDragStop={() => {
        const map = Object.fromEntries(flow.getNodes().map((n) => [n.id, n.position]));
        save(`mc.canvas.${projectId}`, JSON.stringify(map));
      }}
      fitView
      fitViewOptions={{ maxZoom: 1, padding: 0.4 }}
      minZoom={0.3}
      maxZoom={1.5}
      proOptions={{ hideAttribution: true }}
      className="bg-background"
    >
      <Background variant={BackgroundVariant.Dots} gap={16} size={1} color="var(--canvas-dot)" />
      <div className="absolute bottom-4 left-4 z-10 flex flex-col gap-1 rounded-lg border border-border bg-card p-1">
        {[
          { label: "Zoom in", icon: <Plus />, run: () => flow.zoomIn() },
          { label: "Zoom out", icon: <Minus />, run: () => flow.zoomOut() },
          { label: "Fit to screen", icon: <Maximize />, run: () => flow.fitView({ maxZoom: 1, padding: 0.4 }) },
        ].map((b) => (
          <button
            key={b.label}
            title={b.label}
            aria-label={b.label}
            onClick={b.run}
            className="flex size-8 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground [&_svg]:size-4"
          >
            {b.icon}
          </button>
        ))}
      </div>
    </ReactFlow>
  );
}

export function Canvas(props: Parameters<typeof CanvasInner>[0]) {
  return (
    <ReactFlowProvider>
      <CanvasInner {...props} />
    </ReactFlowProvider>
  );
}
