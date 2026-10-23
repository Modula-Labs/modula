import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { DotsVertical, Plus, Server, Trash } from "@/components/ui/icons";
import { Select } from "@/components/ui/Input";
import { Menu, MenuItem } from "@/components/ui/Menu";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, EmptyState, Skeleton, StatusDot } from "@/components/ui/misc";
import { api, type JoinToken, type LocationOption, type NodeInfo } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";
import { relativeTime } from "@/lib/format";

const gb = (b?: number) => (b ? `${(b / 1024 ** 3).toFixed(b >= 10 * 1024 ** 3 ? 0 : 1)} GB` : "—");

/** Instance admins only: the servers that run workloads. */
export function ServersPage() {
  const qc = useQueryClient();
  const nodes = useQuery({ queryKey: ["nodes"], queryFn: api.nodes, refetchInterval: 10_000 });
  const [adding, setAdding] = useState(false);
  const revoke = useMutation({
    mutationFn: (id: string) => api.revokeNode(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["nodes"] }),
  });

  const locations = useQuery({ queryKey: ["locations"], queryFn: api.locations, staleTime: 5 * 60_000 });
  const setLocation = useMutation({
    mutationFn: ({ id, location }: { id: string; location: string | null }) => api.setNodeLocation(id, location),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["nodes"] }),
  });

  return (
    <div>
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-[30px] leading-9 font-normal tracking-[-0.01em]">Servers</h1>
        <Button variant="primary" icon={<Plus />} onClick={() => setAdding(true)}>
          Add server
        </Button>
      </div>
      <p className="mt-2 text-sm text-muted-foreground">
        Each server runs <span className="font-mono">modula-node</span>, which dials out to this instance over mutual TLS. No inbound ports needed.
      </p>

      <div className="mt-8">
        {revoke.error ?? setLocation.error ? <Alert>{(revoke.error ?? setLocation.error)?.message}</Alert> : null}
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
              <Button variant="outline" icon={<Plus />} onClick={() => setAdding(true)}>
                Add server
              </Button>
            }
          />
        ) : (
          <div className="divide-y divide-border rounded-lg border border-border">
            {nodes.data.map((n) => (
              <NodeRow
                key={n.id}
                n={n}
                locations={locations.data ?? []}
                onLocation={(location) => setLocation.mutate({ id: n.id, location })}
                onRevoke={() => revoke.mutate(n.id)}
              />
            ))}
          </div>
        )}
      </div>
      <AddServerDialog open={adding} onOpenChange={setAdding} />
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
  const tone = n.status === "revoked" ? "danger" : n.connected ? "success" : "neutral";
  const label = n.status === "revoked" ? "Revoked" : n.connected ? "Connected" : "Offline";
  const behind = n.connected && n.appliedGeneration < n.desiredGeneration;
  return (
    <div className="flex min-h-16 items-center gap-4 px-4 py-3">
      <Server className="size-5 text-muted-foreground" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate font-medium">{n.name}</span>
          {n.runtime ? <Badge>{n.runtime}</Badge> : null}
          {behind ? <Badge tone="warn">Syncing</Badge> : null}
        </div>
        <div className="truncate text-xs text-muted-foreground">
          {[n.info.os, n.info.arch, n.info.cpus ? `${n.info.cpus} vCPU` : null, gb(n.info.memoryBytes), n.nodeVersion ? `v${n.nodeVersion}` : null]
            .filter(Boolean)
            .join(" · ")}
        </div>
      </div>
      {n.status !== "revoked" ? <LocationSelect n={n} locations={locations} onChange={onLocation} /> : null}
      <div className="flex flex-col items-end gap-0.5">
        <StatusDot tone={tone}>{label}</StatusDot>
        <span className="text-xs text-muted-foreground">{n.lastSeenAt ? `Seen ${relativeTime(n.lastSeenAt)}` : "Never connected"}</span>
      </div>
      {n.status !== "revoked" ? (
        <Menu
          trigger={
            <button aria-label={`Actions for ${n.name}`} className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground">
              <DotsVertical className="size-4" />
            </button>
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
function LocationSelect({ n, locations, onChange }: { n: NodeInfo; locations: LocationOption[]; onChange: (l: string | null) => void }) {
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
        {!known && current ? <option value={current.id}>{current.label}</option> : null}
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

function AddServerDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  const [token, setToken] = useState<JoinToken | null>(null);
  const create = useMutation({ mutationFn: api.createJoinToken, onSuccess: setToken });
  return (
    <Modal
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) {
          setToken(null);
          create.reset();
        }
      }}
      width={600}
      title="Add a server"
      description="Run one command on a fresh Ubuntu or Debian server. It installs Docker, Caddy and modula-node, then joins this instance."
    >
      {create.error ? <Alert>{create.error.message}</Alert> : null}
      {token ? (
        <div className="flex flex-col gap-3">
          <div className="flex items-start gap-2 rounded-lg border border-border bg-background p-3">
            <code className="min-w-0 flex-1 font-mono text-[12.5px] leading-5 break-all">{token.command}</code>
            <CopyButton value={token.command} label="Copy command" />
          </div>
          <details className="text-xs text-muted-foreground">
            <summary className="cursor-pointer select-none hover:text-foreground">modula-node is already installed there</summary>
            <div className="mt-2 flex items-start gap-2 rounded-lg border border-border bg-background p-3">
              <code className="min-w-0 flex-1 font-mono text-[12.5px] leading-5 break-all text-foreground">{token.nodeCommand}</code>
              <CopyButton value={token.nodeCommand} label="Copy join command" />
            </div>
          </details>
          <p className="text-xs text-muted-foreground">
            Single use, expires {relativeTime(token.expiresAt)}. The command pins this instance's certificate ({token.fingerprint.slice(0, 19)}…), so it can't be redirected.
          </p>
          <div className="flex justify-end">
            <Button variant="secondary" onClick={() => onOpenChange(false)}>
              Done
            </Button>
          </div>
        </div>
      ) : (
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button variant="primary" loading={create.isPending} onClick={() => create.mutate()}>
            Create join command
          </Button>
        </div>
      )}
    </Modal>
  );
}
