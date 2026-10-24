import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Checkbox, Radio } from "@/components/ui/Controls";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Skeleton } from "@/components/ui/misc";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/Table";
import { Segmented } from "@/components/ui/Tabs";
import { api, type JoinToken } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";
import { relativeTime } from "@/lib/format";
import {
  euros,
  provisionApi,
  typesIn,
  validServerName,
  type ProvisionedList,
} from "@/lib/provision";
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
            ? "Create a server at Hetzner. It boots, installs Docker, Caddy and modula-node, and adds itself as a node over mutual TLS."
            : "Run one command on a fresh Ubuntu or Debian server. It installs Docker, Caddy and modula-node, then adds it to this instance as a node."
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
              { value: "manual", label: "Manual" },
            ]}
          />
          {provider === "hetzner" ? (
            list.isPending && workspaceId ? (
              <Skeleton className="h-40" />
            ) : connected && workspaceId && list.data ? (
              <HetznerForm
                workspaceId={workspaceId}
                list={list.data}
                onCreated={setCreated}
                onCancel={() => close(false)}
              />
            ) : (
              <Alert tone="info" title="Hetzner is not connected">
                A workspace admin can connect it in Settings → Integrations.
                Until then, add one manually.
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

  const types = useMemo(
    () => typesIn(catalog.data?.serverTypes ?? [], location),
    [catalog.data, location],
  );
  useEffect(() => {
    if (types.length && !types.some((t) => t.name === serverType))
      setServerType(types[0]!.name);
  }, [types, serverType]);

  const chosen = types.find((t) => t.name === serverType);
  const { caps, usage } = list;
  const overCount = usage.servers + 1 > caps.maxServers;
  const overSpend = chosen
    ? usage.monthlyCents + chosen.monthlyCents > caps.maxMonthlyCents
    : false;
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
        <legend className="mb-1.5 text-sm font-bold">Location</legend>
        <div className="grid grid-cols-3 gap-2">
          {catalog.data.locations.map((l) => (
            <button
              key={l.location.id}
              type="button"
              aria-pressed={location === l.location.id}
              onClick={() => setLocation(l.location.id)}
              className={clsx(
                "flex items-center gap-2.5 rounded-lg border px-3 py-2.5 text-left text-sm",
                location === l.location.id
                  ? "border-foreground bg-muted"
                  : "border-border hover:bg-muted/50",
              )}
            >
              <span aria-hidden className="text-base">
                {flagEmoji(l.location.countryCode)}
              </span>
              <span className="min-w-0">
                <span className="block truncate">{l.location.city}</span>
                <span className="block font-mono text-xs text-muted-foreground">
                  {l.region}
                </span>
              </span>
            </button>
          ))}
        </div>
      </fieldset>

      <fieldset className="flex flex-col gap-2">
        <legend className="mb-1.5 text-sm font-bold">Server type</legend>
        {types.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No server types are available in this location.
          </p>
        ) : (
          <div className="max-h-56 overflow-auto rounded-xl">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Type</TableHead>
                  <TableHead className="text-right">vCPU</TableHead>
                  <TableHead className="text-right">RAM</TableHead>
                  <TableHead className="text-right">Disk</TableHead>
                  <TableHead>CPU</TableHead>
                  <TableHead className="text-right">Per month</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {types.map((t) => (
                  <TableRow
                    key={t.name}
                    onClick={() => setServerType(t.name)}
                    data-selected={serverType === t.name}
                    className="cursor-pointer"
                  >
                    <TableCell>
                      <label className="flex items-center gap-2">
                        <Radio
                          name="server-type"
                          value={t.name}
                          checked={serverType === t.name}
                          onChange={() => setServerType(t.name)}
                        />
                        <span className="font-mono">{t.name}</span>
                      </label>
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {t.cores}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {t.memoryGb} GB
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {t.diskGb} GB
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {t.arch === "arm" ? "Arm" : "x86"} · {t.cpuType}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {euros(t.monthlyCents)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
        <p className="text-[13px] text-muted-foreground">
          Prices include VAT, as Hetzner bills them. Traffic beyond the included
          amount is extra.
        </p>
      </fieldset>

      <div className="grid grid-cols-2 gap-4">
        <Field
          label="Name"
          error={
            name && !nameOk ? "Lowercase letters, digits and hyphens" : null
          }
          hint="Hostname and server name."
        >
          {(p) => (
            <Input
              {...p}
              mono
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          )}
        </Field>
        <div className="flex flex-col gap-1.5">
          <span className="text-sm">SSH access</span>
          <Checkbox
            className="h-8"
            checked={ssh}
            onCheckedChange={setSsh}
            label="Open port 22 in the firewall"
          />
          {ssh ? (
            sshKeys.data?.length ? (
              <div className="flex flex-col gap-1">
                {sshKeys.data.map((k) => (
                  <Checkbox
                    key={k.id}
                    checked={keyIds.includes(k.id)}
                    onCheckedChange={(on) =>
                      setKeyIds((ids) =>
                        on ? [...ids, k.id] : ids.filter((i) => i !== k.id),
                      )
                    }
                    label={
                      <>
                        {k.name}{" "}
                        <span className="font-mono text-muted-foreground">
                          {k.fingerprint.slice(0, 17)}
                        </span>
                      </>
                    }
                  />
                ))}
              </div>
            ) : (
              <p className="text-[13px] text-muted-foreground">
                No SSH keys in this Hetzner project; Hetzner emails a root
                password instead.
              </p>
            )
          ) : (
            <p className="text-[13px] text-muted-foreground">
              Only 80, 443 and the TCP proxy ports are open.
            </p>
          )}
        </div>
      </div>

      <div className="flex items-center justify-between gap-4 border-t border-border pt-4">
        <p
          className={clsx(
            "text-[13px]",
            overCount || overSpend ? "text-error" : "text-muted-foreground",
          )}
        >
          {usage.servers} of {caps.maxServers} servers ·{" "}
          {euros(usage.monthlyCents)} of {euros(caps.maxMonthlyCents)} per month
          {chosen ? ` · adds ${euros(chosen.monthlyCents)}` : ""}
          {overCount
            ? " — server limit reached"
            : overSpend
              ? " — over the monthly limit"
              : ""}
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
          <div className="flex items-start gap-2 rounded-lg border border-border bg-muted p-3">
            <code className="min-w-0 flex-1 font-mono text-[13px] leading-5 break-all">
              {token.command}
            </code>
            <CopyButton value={token.command} label="Copy command" />
          </div>
          <details className="text-[13px] text-muted-foreground">
            <summary className="cursor-pointer select-none hover:text-foreground">
              modula-node is already installed there
            </summary>
            <div className="mt-2 flex items-start gap-2 rounded-lg border border-border bg-muted p-3">
              <code className="min-w-0 flex-1 font-mono text-[13px] leading-5 break-all text-foreground">
                {token.nodeCommand}
              </code>
              <CopyButton value={token.nodeCommand} label="Copy command" />
            </div>
          </details>
          <p className="text-[13px] text-muted-foreground">
            Single use, expires {relativeTime(token.expiresAt)}. The command
            pins this instance's certificate ({token.fingerprint.slice(0, 19)}
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
          <Button
            variant="primary"
            loading={create.isPending}
            onClick={() => create.mutate()}
          >
            Create command
          </Button>
        </div>
      )}
    </div>
  );
}
