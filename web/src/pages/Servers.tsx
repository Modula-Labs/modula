import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { DotsVertical, Plus, Server, Trash } from "@/components/ui/icons";
import { Segmented } from "@/components/ui/Tabs";
import { Menu, MenuItem } from "@/components/ui/Menu";
import {
  Alert,
  Badge,
  EmptyState,
  Skeleton,
  StatusDot,
} from "@/components/ui/misc";
import { api, type NodeInfo } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";
import { useCurrentWorkspace, useMe } from "@/lib/workspace";
import { SelfBackupCard } from "./BackupStorage";
import { relativeTime } from "@/lib/format";
import { AddServerDialog } from "./servers/AddServerDialog";
import { NetworkPanel } from "./servers/NetworkPanel";
import { ProvisionedServers } from "./servers/ProvisionedServers";

export const gb = (b?: number) =>
  b ? `${(b / 1024 ** 3).toFixed(b >= 10 * 1024 ** 3 ? 0 : 1)} GB` : "—";

/** Connected / Offline / Revoked, as a status dot. */
export function nodeStatus(n: NodeInfo): {
  tone: "success" | "danger" | "neutral";
  label: string;
} {
  if (n.status === "revoked") return { tone: "danger", label: "Revoked" };
  return n.connected
    ? { tone: "success", label: "Connected" }
    : { tone: "neutral", label: "Offline" };
}

/** Flag and city of where the server runs. */
export function NodeLocation({ n }: { n: NodeInfo }) {
  if (!n.location) return null;
  return (
    <span className="flex items-center gap-1.5 text-sm whitespace-nowrap">
      <span aria-hidden className="text-base">
        {flagEmoji(n.location.countryCode)}
      </span>
      {n.location.city || n.location.label}
    </span>
  );
}

/** Instance admins only: the servers that run workloads. */
export function ServersPage() {
  const qc = useQueryClient();
  const { data: me } = useMe();
  const [workspace] = useCurrentWorkspace(me);
  const nodes = useQuery({
    queryKey: ["nodes"],
    queryFn: api.nodes,
    refetchInterval: 10_000,
  });
  const [adding, setAdding] = useState(false);
  const [tab, setTab] = useState<"servers" | "network">("servers");
  const revoke = useMutation({
    mutationFn: (id: string) => api.revokeNode(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["nodes"] }),
  });

  return (
    <div>
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-2xl leading-8 font-bold">Servers</h1>
        <Button
          variant="primary"
          icon={<Plus />}
          onClick={() => setAdding(true)}
        >
          Add server
        </Button>
      </div>
      <p className="mt-1 text-sm text-muted-foreground">
        Each server runs <span className="font-mono">modula-node</span>, which
        dials out to this instance over mutual TLS. No inbound ports needed.
      </p>

      <div className="mt-6">
        <Segmented
          label="View"
          value={tab}
          onChange={setTab}
          options={[
            { value: "servers", label: "Servers" },
            { value: "network", label: "Network" },
          ]}
        />
      </div>

      {tab === "network" ? (
        <div className="mt-4">
          <NetworkPanel />
        </div>
      ) : (
        <>
          <div className="mt-4 flex flex-col gap-4">
            {revoke.error ? <Alert>{revoke.error.message}</Alert> : null}
            {nodes.isPending ? (
              <Skeleton className="h-32" />
            ) : nodes.error ? (
              <Alert>{nodes.error.message}</Alert>
            ) : nodes.data.length === 0 ? (
              <EmptyState
                icon={<Server />}
                title="No servers yet"
                body="Add a server to start deploying. Any Linux machine with Docker works, including this one."
                action={
                  <Button
                    variant="outline"
                    icon={<Plus />}
                    onClick={() => setAdding(true)}
                  >
                    Add server
                  </Button>
                }
              />
            ) : (
              <div className="divide-y divide-border rounded-xl border border-border">
                {nodes.data.map((n) => (
                  <NodeRow
                    key={n.id}
                    n={n}
                    onRevoke={() => revoke.mutate(n.id)}
                  />
                ))}
              </div>
            )}
          </div>
          <SelfBackupCard workspaceId={workspace?.id} />
          <ProvisionedServers workspaceId={workspace?.id} />
        </>
      )}
      <AddServerDialog
        open={adding}
        onOpenChange={setAdding}
        workspaceId={workspace?.id}
      />
    </div>
  );
}

function NodeRow({ n, onRevoke }: { n: NodeInfo; onRevoke: () => void }) {
  const { tone, label } = nodeStatus(n);
  const behind = n.connected && n.appliedGeneration < n.desiredGeneration;
  return (
    <div className="flex min-h-16 items-center gap-2 pr-3 hover:bg-muted/50">
      <Link
        to="/servers/$nodeId"
        params={{ nodeId: n.id }}
        className="flex min-w-0 flex-1 items-center gap-4 py-2.5 pl-3"
      >
        <Server className="size-5 text-muted-foreground" />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-sm">{n.name}</span>
            {n.runtime ? <Badge>{n.runtime}</Badge> : null}
            {behind ? <Badge tone="warn">Syncing</Badge> : null}
          </div>
          <div className="truncate text-[13px] text-muted-foreground">
            {[
              n.info.os,
              n.info.arch,
              n.info.cpus ? `${n.info.cpus} vCPU` : null,
              gb(n.info.memoryBytes),
              n.nodeVersion ? `v${n.nodeVersion}` : null,
            ]
              .filter(Boolean)
              .join(" · ")}
          </div>
        </div>
        <NodeLocation n={n} />
        <div className="flex flex-col items-end gap-0.5">
          <StatusDot tone={tone}>{label}</StatusDot>
          <span className="text-[13px] text-muted-foreground">
            {n.lastSeenAt
              ? `Seen ${relativeTime(n.lastSeenAt)}`
              : "Never connected"}
          </span>
        </div>
      </Link>
      {n.status !== "revoked" ? (
        <Menu
          trigger={
            <Button
              variant="ghost"
              size="icon"
              aria-label={`Actions for ${n.name}`}
            >
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
  );
}
