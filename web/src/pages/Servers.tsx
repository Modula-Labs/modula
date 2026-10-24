import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { DotsVertical, Plus, Server, Trash } from "@/components/ui/icons";
import { Select } from "@/components/ui/Input";
import { Segmented } from "@/components/ui/Tabs";
import { Menu, MenuItem } from "@/components/ui/Menu";
import {
  Alert,
  Badge,
  EmptyState,
  Skeleton,
  StatusDot,
} from "@/components/ui/misc";
import { api, type LocationOption, type NodeInfo } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";
import { useCurrentWorkspace, useMe } from "@/lib/workspace";
import { SelfBackupCard } from "./BackupStorage";
import { relativeTime } from "@/lib/format";
import { AddServerDialog } from "./servers/AddServerDialog";
import { NetworkPanel } from "./servers/NetworkPanel";
import { ProvisionedServers } from "./servers/ProvisionedServers";

const gb = (b?: number) =>
  b ? `${(b / 1024 ** 3).toFixed(b >= 10 * 1024 ** 3 ? 0 : 1)} GB` : "—";

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

  const locations = useQuery({
    queryKey: ["locations"],
    queryFn: api.locations,
    staleTime: 5 * 60_000,
  });
  const setLocation = useMutation({
    mutationFn: ({ id, location }: { id: string; location: string | null }) =>
      api.setNodeLocation(id, location),
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
            {(revoke.error ?? setLocation.error) ? (
              <Alert>{(revoke.error ?? setLocation.error)?.message}</Alert>
            ) : null}
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
                    locations={locations.data ?? []}
                    onLocation={(location) =>
                      setLocation.mutate({ id: n.id, location })
                    }
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

function NodeRow({
  n,
  locations,
  onLocation,
  onRevoke,
}: {
  n: NodeInfo;
  locations: LocationOption[];
  onLocation: (location: string | null) => void;
  onRevoke: () => void;
}) {
  const tone =
    n.status === "revoked" ? "danger" : n.connected ? "success" : "neutral";
  const label =
    n.status === "revoked" ? "Revoked" : n.connected ? "Connected" : "Offline";
  const behind = n.connected && n.appliedGeneration < n.desiredGeneration;
  return (
    <div className="flex min-h-16 items-center gap-4 px-3 py-2.5 hover:bg-muted/50">
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
      {n.status !== "revoked" ? (
        <LocationSelect n={n} locations={locations} onChange={onLocation} />
      ) : null}
      <div className="flex flex-col items-end gap-0.5">
        <StatusDot tone={tone}>{label}</StatusDot>
        <span className="text-[13px] text-muted-foreground">
          {n.lastSeenAt
            ? `Seen ${relativeTime(n.lastSeenAt)}`
            : "Never connected"}
        </span>
      </div>
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

/** Instance admins pick where a server lives; it shows on every service it runs. */
function LocationSelect({
  n,
  locations,
  onChange,
}: {
  n: NodeInfo;
  locations: LocationOption[];
  onChange: (l: string | null) => void;
}) {
  const providers = [...new Set(locations.map((l) => l.provider))];
  const current = n.location;
  const known = !current || locations.some((l) => l.id === current.id);
  return (
    <div className="flex items-center gap-2">
      <span aria-hidden className="text-base">
        {flagEmoji(current?.countryCode)}
      </span>
      <Select
        aria-label={`Location of ${n.name}`}
        className="w-44"
        value={current?.id ?? ""}
        onChange={(e) => onChange(e.target.value || null)}
      >
        <option value="">No location</option>
        {!known && current ? (
          <option value={current.id}>{current.label}</option>
        ) : null}
        {providers.map((p) => (
          <optgroup key={p} label={p}>
            {locations
              .filter((l) => l.provider === p)
              .map((l) => (
                <option key={l.id} value={l.id}>
                  {l.label}
                </option>
              ))}
          </optgroup>
        ))}
      </Select>
    </div>
  );
}
