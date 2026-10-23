import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Skeleton } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import { api, type JoinToken } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";
import { relativeTime } from "@/lib/format";
import { euros, provisionApi, typesIn, validServerName, type ProvisionedList } from "@/lib/provision";
import { AddServerProgress } from "./AddServerProgress";

type Provider = "hetzner" | "manual";

/** Add a server: create one at Hetzner (when connected), or join any machine by hand. */
export function AddServerDialog({
  open,
  onOpenChange,
  workspaceId,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  workspaceId: string | undefined;
}) {
  const list = useQuery({
    queryKey: ["provisioned", workspaceId],
    queryFn: () => provisionApi.servers(workspaceId!),
    enabled: open && !!workspaceId,
  });
  const connected = list.data?.hetznerConnected ?? false;
  const [provider, setProvider] = useState<Provider>("manual");
  const [created, setCreated] = useState<string | null>(null);
  useEffect(() => {
    if (open) setProvider(connected ? "hetzner" : "manual");
  }, [open, connected]);

  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) setCreated(null);
  };

  return (
    <Modal
      open={open}
      onOpenChange={close}
      width={provider === "hetzner" && !created ? 720 : 600}
      title={created ? "Adding server" : "Add a server"}
      description={
        created
          ? "Hetzner is creating the server. It installs modula-node on first boot and joins this instance."
          : provider === "hetzner"
            ? "Create a server at Hetzner. It boots, installs Docker, Caddy and modula-node, and joins over mutual TLS."
            : "Run one command on a fresh Ubuntu or Debian server. It installs Docker, Caddy and modula-node, then joins this instance."
      }
    >
      {created ? (
        <AddServerProgress serverId={created} onDone={() => close(false)} />
      ) : (
        <div className="flex flex-col gap-4">
          <Segmented<Provider>
            label="Provider"
            value={provider}
            onChange={setProvider}
            options={[
              { value: "hetzner", label: "Hetzner Cloud" },
              { value: "manual", label: "Manual join" },
            ]}
          />
          {provider === "hetzner" ? (
            list.isPending && workspaceId ? (
              <Skeleton className="h-40" />
            ) : connected && workspaceId && list.data ? (
              <HetznerForm workspaceId={workspaceId} list={list.data} onCreated={setCreated} onCancel={() => close(false)} />
            ) : (
              <Alert tone="info" title="Hetzner is not connected">
                A workspace admin can connect it in Settings → Integrations. Until then, use a manual join.
              </Alert>
            )
          ) : (
            <ManualJoin onDone={() => close(false)} />
          )}
        </div>
      )}
    </Modal>
  );
}

function HetznerForm({
  workspaceId,
  list,
  onCreated,
  onCancel,
}: {
  workspaceId: string;
  list: ProvisionedList;
  onCreated: (id: string) => void;
  onCancel: () => void;
}) {
  const qc = useQueryClient();
  const catalog = useQuery({
    queryKey: ["hetzner-catalog", workspaceId],
    queryFn: () => provisionApi.hetznerCatalog(workspaceId),
    staleTime: 5 * 60_000,
  });
  const sshKeys = useQuery({
    queryKey: ["hetzner-ssh-keys", workspaceId],
    queryFn: () => provisionApi.hetznerSshKeys(workspaceId),
    staleTime: 60_000,
  });
  const [location, setLocation] = useState("hetzner:fsn1");
  const [serverType, setServerType] = useState<string | null>(null);
  const [name, setName] = useState(() => `worker-${list.servers.length + 1}`);
  const [ssh, setSsh] = useState(false);
  const [keyIds, setKeyIds] = useState<number[]>([]);

  const types = useMemo(() => typesIn(catalog.data?.serverTypes ?? [], location), [catalog.data, location]);
  useEffect(() => {
    if (types.length && !types.some((t) => t.name === serverType)) setServerType(types[0]!.name);
  }, [types, serverType]);

  const chosen = types.find((t) => t.name === serverType);
  const { caps, usage } = list;
  const overCount = usage.servers + 1 > caps.maxServers;
  const overSpend = chosen ? usage.monthlyCents + chosen.monthlyCents > caps.maxMonthlyCents : false;
  const nameOk = validServerName(name);

  const create = useMutation({
    mutationFn: () =>
      provisionApi.provision(workspaceId, {
        location,
        serverType: serverType!,
        name,
        ssh,
        sshKeyIds: ssh ? keyIds : [],
      }),
    onSuccess: (s) => {
      qc.invalidateQueries({ queryKey: ["provisioned", workspaceId] });
      onCreated(s.id);
    },
  });

  if (catalog.isPending) return <Skeleton className="h-64" />;
  if (catalog.error) return <Alert>{catalog.error.message}</Alert>;

  return (
    <div className="flex flex-col gap-5">
      {create.error ? <Alert>{create.error.message}</Alert> : null}

      <fieldset className="flex flex-col gap-2">
        <legend className="mb-1.5 text-sm font-medium">Location</legend>
        <div className="grid grid-cols-3 gap-2">
          {catalog.data.locations.map((l) => (
            <button
              key={l.location.id}
              type="button"
              aria-pressed={location === l.location.id}
              onClick={() => setLocation(l.location.id)}
              className={clsx(
                "flex items-center gap-2.5 rounded-lg border px-3 py-2 text-left text-sm",
                location === l.location.id ? "border-foreground bg-accent" : "border-border hover:bg-accent",
              )}
            >
              <span aria-hidden className="text-base">
                {flagEmoji(l.location.countryCode)}
              </span>
              <span className="min-w-0">
                <span className="block truncate font-medium">{l.location.city}</span>
                <span className="block font-mono text-xs text-muted-foreground">{l.region}</span>
              </span>
            </button>
          ))}
        </div>
      </fieldset>

      <fieldset className="flex flex-col gap-2">
        <legend className="mb-1.5 text-sm font-medium">Server type</legend>
        {types.length === 0 ? (
          <p className="text-sm text-muted-foreground">No server types are available in this location.</p>
        ) : (
          <div className="max-h-56 overflow-auto rounded-lg border border-border">
            <table className="w-full text-sm">
              <thead className="sticky top-0 bg-card text-left text-xs text-muted-foreground">
                <tr>
                  <th className="px-3 py-2 font-medium">Type</th>
                  <th className="px-3 py-2 text-right font-medium">vCPU</th>
                  <th className="px-3 py-2 text-right font-medium">RAM</th>
                  <th className="px-3 py-2 text-right font-medium">Disk</th>
                  <th className="px-3 py-2 font-medium">CPU</th>
                  <th className="px-3 py-2 text-right font-medium">Per month</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-border">
                {types.map((t) => (
                  <tr
                    key={t.name}
                    onClick={() => setServerType(t.name)}
                    className={clsx("cursor-pointer", serverType === t.name ? "bg-accent" : "hover:bg-accent/60")}
                  >
                    <td className="px-3 py-2">
                      <label className="flex items-center gap-2">
                        <input
                          type="radio"
                          name="server-type"
                          checked={serverType === t.name}
                          onChange={() => setServerType(t.name)}
                          className="size-4 accent-foreground"
                        />
                        <span className="font-mono">{t.name}</span>
                      </label>
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">{t.cores}</td>
                    <td className="px-3 py-2 text-right tabular-nums">{t.memoryGb} GB</td>
                    <td className="px-3 py-2 text-right tabular-nums">{t.diskGb} GB</td>
                    <td className="px-3 py-2 text-muted-foreground">
                      {t.arch === "arm" ? "Arm" : "x86"} · {t.cpuType}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">{euros(t.monthlyCents)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <p className="text-xs text-muted-foreground">
          Prices include VAT, as Hetzner bills them. Traffic beyond the included amount is extra.
        </p>
      </fieldset>

      <div className="grid grid-cols-2 gap-4">
        <Field label="Name" error={name && !nameOk ? "Lowercase letters, digits and hyphens" : null} hint="Hostname and server name.">
          {(p) => <Input {...p} mono value={name} onChange={(e) => setName(e.target.value)} />}
        </Field>
        <div className="flex flex-col gap-1.5">
          <span className="text-sm font-medium">SSH access</span>
          <label className="flex h-9 items-center gap-2.5 text-sm">
            <input type="checkbox" checked={ssh} onChange={(e) => setSsh(e.target.checked)} className="size-4 accent-foreground" />
            Open port 22 in the firewall
          </label>
          {ssh ? (
            sshKeys.data?.length ? (
              <div className="flex flex-col gap-1">
                {sshKeys.data.map((k) => (
                  <label key={k.id} className="flex items-center gap-2 text-xs">
                    <input
                      type="checkbox"
                      checked={keyIds.includes(k.id)}
                      onChange={(e) => setKeyIds((ids) => (e.target.checked ? [...ids, k.id] : ids.filter((i) => i !== k.id)))}
                      className="size-3.5 accent-foreground"
                    />
                    {k.name} <span className="font-mono text-muted-foreground">{k.fingerprint.slice(0, 17)}</span>
                  </label>
                ))}
              </div>
            ) : (
              <p className="text-xs text-muted-foreground">No SSH keys in this Hetzner project; Hetzner emails a root password instead.</p>
            )
          ) : (
            <p className="text-xs text-muted-foreground">Only 80, 443 and the TCP proxy ports are open.</p>
          )}
        </div>
      </div>

      <div className="flex items-center justify-between gap-4 border-t border-border pt-4">
        <p className={clsx("text-xs", overCount || overSpend ? "text-error" : "text-muted-foreground")}>
          {usage.servers} of {caps.maxServers} servers · {euros(usage.monthlyCents)} of {euros(caps.maxMonthlyCents)} per month
          {chosen ? ` · adds ${euros(chosen.monthlyCents)}` : ""}
          {overCount ? " — server limit reached" : overSpend ? " — over the monthly limit" : ""}
        </p>
        <div className="flex gap-2">
          <Button variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button
            variant="primary"
            loading={create.isPending}
            disabled={!chosen || !nameOk || overCount || overSpend}
            onClick={() => create.mutate()}
          >
            Create server
          </Button>
        </div>
      </div>
    </div>
  );
}

function ManualJoin({ onDone }: { onDone: () => void }) {
  const [token, setToken] = useState<JoinToken | null>(null);
  const create = useMutation({
    mutationFn: api.createJoinToken,
    onSuccess: setToken,
  });
  return (
    <div className="flex flex-col gap-3">
      {create.error ? <Alert>{create.error.message}</Alert> : null}
      {token ? (
        <>
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
            Single use, expires {relativeTime(token.expiresAt)}. The command pins this instance's certificate (
            {token.fingerprint.slice(0, 19)}
            …), so it can't be redirected.
          </p>
          <div className="flex justify-end">
            <Button variant="secondary" onClick={onDone}>
              Done
            </Button>
          </div>
        </>
      ) : (
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onDone}>
            Cancel
          </Button>
          <Button variant="primary" loading={create.isPending} onClick={() => create.mutate()}>
            Create join command
          </Button>
        </div>
      )}
    </div>
  );
}
