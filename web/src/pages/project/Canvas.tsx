import {
  Background,
  BackgroundVariant,
  Handle,
  Position,
  ReactFlow,
  ReactFlowProvider,
  getBezierPath,
  useInternalNode,
  useNodesState,
  useReactFlow,
  type Edge,
  type EdgeProps,
  type InternalNode,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/base.css";
import { clsx } from "clsx";
import { useEffect, useMemo, useRef, useState } from "react";
import { Maximize, Minus, Plus } from "@/components/ui/icons";
import { ServiceIcon } from "@/components/ui/ServiceIcon";
import { StatusDot } from "@/components/ui/misc";
import type { DeploymentStatus } from "@/lib/api";
import { serviceState } from "@/lib/status";
import { load, save } from "@/lib/storage";
import { dependencyPairs, edgeAnchors, layeredLayout, type Rect, type Side } from "./edges";

export interface CanvasService {
  id: string;
  name: string;
  domain?: string;
  /** `owner/name` for services built from GitHub. */
  repo?: string;
  /** Docker image for image-based services. */
  image?: string | null;
  status: DeploymentStatus | null;
  /** Detected framework/runtime, if known. */
  stack?: string | null;
  /** Ids of the services this one references; drawn as edges pointing at them. */
  dependsOn?: string[];
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
        <ServiceIcon stack={service.stack} image={service.image} repo={service.repo} />
        <span className="truncate text-base font-semibold">{service.name}</span>
      </div>
      <div className="mt-1 truncate text-sm text-muted-foreground">{service.domain ?? service.repo ?? " "}</div>
      <div className="mt-auto">
        <StatusDot tone={state.tone}>{state.label}</StatusDot>
      </div>
      <Handle type="source" position={Position.Right} className="!opacity-0" />
    </div>
  );
}

const nodeTypes = { service: ServiceCard };

type DependencyEdge = Edge<{ active: boolean }, "dependency">;
const sidePosition: Record<Side, Position> = {
  left: Position.Left,
  right: Position.Right,
  top: Position.Top,
  bottom: Position.Bottom,
};

const rectOf = (n: InternalNode): Rect => ({
  x: n.internals.positionAbsolute.x,
  y: n.internals.positionAbsolute.y,
  w: n.measured.width ?? 288,
  h: n.measured.height ?? 144,
});

/** Consumer -> dependency: a thin dashed curve with an arrowhead on the
 *  dependency. Endpoints are derived from the live node rects, so it follows
 *  cards while they are dragged, panned and zoomed. */
function DependencyEdgeView({ id, source, target, data }: EdgeProps<DependencyEdge>) {
  const a = useInternalNode(source);
  const b = useInternalNode(target);
  if (!a || !b) return null;
  const { source: s, target: t } = edgeAnchors(rectOf(a), rectOf(b));
  const [path] = getBezierPath({
    sourceX: s.x,
    sourceY: s.y,
    sourcePosition: sidePosition[s.side],
    targetX: t.x,
    targetY: t.y,
    targetPosition: sidePosition[t.side],
  });
  const active = data?.active ?? false;
  const color = active ? "var(--foreground)" : "var(--muted-foreground)";
  const marker = `dep-arrow-${id}`;
  return (
    <>
      <defs>
        <marker id={marker} viewBox="0 0 10 10" refX="9" refY="5" markerWidth="10" markerHeight="10" markerUnits="userSpaceOnUse" orient="auto">
          <path d="M1 1.5 L9 5 L1 8.5" fill="none" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" style={{ stroke: color }} />
        </marker>
      </defs>
      <path
        d={path}
        fill="none"
        strokeDasharray="5 4"
        strokeWidth={active ? 1.75 : 1.25}
        markerEnd={`url(#${marker})`}
        style={{ stroke: color, opacity: active ? 0.9 : 0.5, transition: "opacity 120ms, stroke 120ms" }}
      />
    </>
  );
}

const edgeTypes = { dependency: DependencyEdgeView };

/** Card positions are kept per project in this browser until the server stores layouts. */
function savedPositions(projectId: string): Record<string, { x: number; y: number }> {
  try {
    return JSON.parse(load(`mc.canvas.${projectId}`) ?? "{}");
  } catch {
    return {};
  }
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
  const box = useRef<HTMLDivElement>(null);
  const [hovered, setHovered] = useState<string>();

  const edges = useMemo<DependencyEdge[]>(
    () =>
      dependencyPairs(services).map(([from, to]) => ({
        id: `${from}>${to}`,
        type: "dependency" as const,
        source: from,
        target: to,
        selectable: false,
        focusable: false,
        data: { active: [hovered, selected].some((x) => x !== undefined && (x === from || x === to)) },
      })),
    [services, hovered, selected],
  );

  // The service sheet covers the right ~60%: keep the selected card in view
  // to its left, like Railway, without changing the zoom.
  useEffect(() => {
    if (!selected || !box.current) return;
    const node = flow.getNode(selected);
    if (!node) return;
    const { width } = box.current.getBoundingClientRect();
    const zoom = flow.getZoom();
    const visible = width * 0.4;
    const cardW = node.measured?.width ?? 288;
    const cardH = node.measured?.height ?? 144;
    const screenX = node.position.x * zoom + flow.getViewport().x;
    if (screenX >= 16 && screenX + cardW * zoom <= visible - 16) return;
    void flow.setCenter(node.position.x + cardW / 2 + (width / 2 - visible / 2) / zoom, node.position.y + cardH / 2, {
      zoom,
      duration: 250,
    });
    // Only when the selection changes, not on every data refresh.
  }, [selected]);

  useEffect(() => {
    const saved = savedPositions(projectId);
    const layout = layeredLayout(services);
    setNodes((prev) =>
      services.map((s) => ({
        id: s.id,
        type: "service" as const,
        position: prev.find((n) => n.id === s.id)?.position ?? saved[s.id] ?? layout[s.id] ?? { x: 0, y: 0 },
        data: { service: s, selected: s.id === selected },
      })),
    );
  }, [projectId, services, selected, setNodes]);

  return (
    <div ref={box} className="h-full w-full">
      <ReactFlow
        nodes={nodes}
        edges={edges}
        nodeTypes={nodeTypes}
        edgeTypes={edgeTypes}
        onNodeMouseEnter={(_, n) => setHovered(n.id)}
        onNodeMouseLeave={() => setHovered(undefined)}
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
    </div>
  );
}

export function Canvas(props: Parameters<typeof CanvasInner>[0]) {
  return (
    <ReactFlowProvider>
      <CanvasInner {...props} />
    </ReactFlowProvider>
  );
}
