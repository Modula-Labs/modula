import { useQuery } from "@tanstack/react-query";
import {
  Alert,
  Badge,
  EmptyState,
  Skeleton,
  StatusDot,
} from "@/components/ui/misc";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/Table";
import { Server } from "@/components/ui/icons";
import { api, type MeshNetwork } from "@/lib/api";
import { relativeTime } from "@/lib/format";

type Link = MeshNetwork["links"][number];

/** A tunnel counts as up when its last handshake is recent (WireGuard re-keys every 2 min). */
export const HANDSHAKE_FRESH_SECS = 180;

export function linkState(
  l: Link,
  nowSecs = Date.now() / 1000,
): "up" | "waiting" | "down" {
  if (l.lastHandshake && nowSecs - l.lastHandshake < HANDSHAKE_FRESH_SECS)
    return "up";
  if (l.path === "none") return "waiting";
  return "down";
}

export function formatLatency(ms: number | null): string {
  if (ms == null) return "—";
  return ms < 10 ? `${ms.toFixed(1)} ms` : `${Math.round(ms)} ms`;
}

/** The private network: every pair of servers, how they connect and how fast. */
export function NetworkPanel() {
  const net = useQuery({
    queryKey: ["mesh-network"],
    queryFn: api.meshNetwork,
    refetchInterval: 15_000,
  });
  if (net.isPending) return <Skeleton className="h-32" />;
  if (net.error) return <Alert>{net.error.message}</Alert>;
  const { nodes, links } = net.data;
  const name = (id: string) => nodes.find((n) => n.id === id)?.name ?? id;
  const inMesh = nodes.filter((n) => n.subnet);
  const outside = nodes.filter((n) => !n.subnet);

  return (
    <div className="flex flex-col gap-4">
      <p className="text-sm text-muted-foreground">
        Servers reach each other over an encrypted WireGuard network: over a
        provider's private network when two share one, otherwise over their
        public addresses. Services find each other at{" "}
        <span className="font-mono">service.modula.internal</span>, and only
        services in the same environment can talk.
      </p>
      {inMesh.length < 2 ? (
        <EmptyState
          icon={<Server />}
          title={
            inMesh.length === 0
              ? "No servers in the private network"
              : "One server so far"
          }
          body="Add another server: it joins the private network on its own, and replicas can then run on both."
        />
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>From</TableHead>
              <TableHead>To</TableHead>
              <TableHead>Path</TableHead>
              <TableHead>Latency</TableHead>
              <TableHead>Tunnel</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {links.map((l) => {
              const state = linkState(l);
              return (
                <TableRow key={`${l.from}-${l.to}`}>
                  <TableCell>{name(l.from)}</TableCell>
                  <TableCell>{name(l.to)}</TableCell>
                  <TableCell>
                    <span className="flex items-center gap-2">
                      {l.path === "private" ? (
                        <Badge tone="success">Private</Badge>
                      ) : l.path === "public" ? (
                        <Badge>Public</Badge>
                      ) : (
                        <Badge tone="warn">Inbound only</Badge>
                      )}
                      {l.endpoint ? (
                        <span className="font-mono text-[13px] text-muted-foreground">
                          {l.endpoint}
                        </span>
                      ) : null}
                    </span>
                  </TableCell>
                  <TableCell className="tabular-nums">
                    {formatLatency(l.latencyMs)}
                  </TableCell>
                  <TableCell>
                    <StatusDot
                      tone={
                        state === "up"
                          ? "success"
                          : state === "waiting"
                            ? "neutral"
                            : "danger"
                      }
                    >
                      {state === "up"
                        ? `Up · ${relativeTime(new Date((l.lastHandshake ?? 0) * 1000).toISOString())}`
                        : state === "waiting"
                          ? "Waiting for the other side"
                          : "No handshake"}
                    </StatusDot>
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      )}
      {inMesh.length > 0 ? (
        <div className="flex flex-wrap gap-x-6 gap-y-1 text-[13px] text-muted-foreground">
          {inMesh.map((n) => (
            <span key={n.id}>
              {n.name} <span className="font-mono">{n.subnet}</span>
            </span>
          ))}
        </div>
      ) : null}
      {outside.length > 0 ? (
        <Alert tone="warn" title="Not in the private network">
          {outside.map((n) => n.name).join(", ")}: these servers run without
          WireGuard (no WireGuard in the kernel).
          Services on them can't reach services on other servers privately.
        </Alert>
      ) : null}
    </div>
  );
}
