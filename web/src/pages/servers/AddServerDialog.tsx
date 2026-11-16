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
  ROOT_DISK,
  defaultLocation,
  hasRootDisk,
  keyRegion,
  monthlyUsd,
  parseDiskGb,
  priceWithDisk,
  provisionApi,
  providerLabel,
  providerName,
  providerOf,
  typesIn,
  usd,
  validServerName,
  type ProvisionRequest,
  type ProvisionedList,
} from "@/lib/provision";
import { AddServerProgress } from "./AddServerProgress";

/** A connected provider's kind (`hetzner`, `aws`), or adding a machine by hand. */
const MANUAL = "manual";

/** Add a server: create one at a connected cloud provider, or join any machine by hand. */
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
  const kinds = list.data?.connectedProviders.map((p) => p.kind) ?? [];
  const kindsKey = kinds.join(",");
  const [provider, setProvider] = useState<string>(MANUAL);
  const [created, setCreated] = useState<string | null>(null);
  useEffect(() => {
    if (open) setProvider(kindsKey.split(",")[0] || MANUAL);
  }, [open, kindsKey]);

  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) setCreated(null);
  };
  const cloud = provider !== MANUAL;

  return (
    <Modal
      open={open}
      onOpenChange={close}
      width={cloud && !created ? 720 : 600}
      title={created ? "Adding server" : "Add a server"}
      description={
        created
          ? `${providerName(provider)} is creating the server. It installs modula-node on first boot and joins this instance.`
          : cloud
            ? `Create a server on ${providerLabel(provider)}. It boots, installs Docker, Caddy and modula-node, and adds itself as a node over mutual TLS.`
            : "Run one command on a fresh Ubuntu or Debian server. It installs Docker, Caddy and modula-node, then adds it to this instance as a node."
      }
    >
      {created ? (
        <AddServerProgress serverId={created} onDone={() => close(false)} />
      ) : (
        <div className="flex flex-col gap-4">
          {kinds.length ? (
            <Segmented
              label="Provider"
              value={provider}
              onChange={setProvider}
              options={[
                ...kinds.map((k) => ({ value: k, label: providerLabel(k) })),
                { value: MANUAL, label: "Manual" },
              ]}
            />
          ) : null}
          {cloud ? (
            list.isPending && workspaceId ? (
              <Skeleton className="h-40" />
            ) : providerOf(list.data, provider) && workspaceId && list.data ? (
              <ProviderForm
                key={provider}
                kind={provider}
                workspaceId={workspaceId}
                list={list.data}
                onCreated={setCreated}
                onCancel={() => close(false)}
              />
            ) : (
              <Alert tone="info" title={`${providerLabel(provider)} is not connected`}>
                A workspace admin can connect it in Settings → Integrations.
                Until then, add one manually.
              </Alert>
            )
          ) : (
            <>
              {list.data && !kinds.length ? (
                <p className="text-[13px] text-muted-foreground">
                  To create servers from here, a workspace admin can connect
                  Hetzner Cloud or AWS EC2 in Settings → Integrations.
                </p>
              ) : null}
              <ManualJoin onDone={() => close(false)} />
            </>
          )}
        </div>
      )}
    </Modal>
  );
}

/** What each provider adds under the price table. */
function priceNote(kind: string): string {
  return kind === "aws"
    ? "On-demand prices in US dollars: compute, the root disk and an Elastic IP. The Elastic IP is included, so the server keeps its address through restarts and resizes; the address shows once AWS has attached it. Data transfer is extra."
    : "Approximate: Hetzner bills in euros, VAT included. Traffic beyond the included amount is extra.";
}

/** Shown in place of the key list when the provider has no SSH keys. */
function noKeysNote(kind: string): string {
  return kind === "aws"
    ? "No key pairs in this AWS account, so nothing could log in over SSH. Import one in the EC2 console (Key Pairs) first."
    : kind === "hetzner"
      ? "No SSH keys in this Hetzner project; Hetzner emails a root password instead."
      : "No SSH keys at this provider.";
}

function ProviderForm({
  kind,
  workspaceId,
  list,
  onCreated,
  onCancel,
}: {
  kind: string;
  workspaceId: string;
  list: ProvisionedList;
  onCreated: (id: string) => void;
  onCancel: () => void;
}) {
  const qc = useQueryClient();
  const catalog = useQuery({
    queryKey: [`${kind}-catalog`, workspaceId],
    queryFn: () => provisionApi.catalog(workspaceId, kind),
    staleTime: 5 * 60_000,
  });
  const sshKeys = useQuery({
    queryKey: [`${kind}-ssh-keys`, workspaceId],
    queryFn: () => provisionApi.sshKeys(workspaceId, kind),
    staleTime: 60_000,
  });
  const [picked, setPicked] = useState<string | null>(null);
  const [serverType, setServerType] = useState<string | null>(null);
  const [name, setName] = useState(() => `worker-${list.servers.length + 1}`);
  const [diskText, setDiskText] = useState(String(ROOT_DISK.default));
  const [ssh, setSsh] = useState(false);
  const [keyIds, setKeyIds] = useState<string[]>([]);

  const locations = catalog.data?.locations ?? [];
  const location = locations.some((l) => l.location.id === picked)
    ? picked
    : defaultLocation(kind, locations);
  const types = useMemo(
    () => typesIn(catalog.data?.serverTypes ?? [], location ?? ""),
    [catalog.data, location],
  );
  useEffect(() => {
    if (types.length && !types.some((t) => t.name === serverType))
      setServerType(types[0]!.name);
  }, [types, serverType]);

  const chosen = types.find((t) => t.name === serverType);
  const rootDisk = hasRootDisk(kind);
  const disk = parseDiskGb(diskText);
  const diskOk = !rootDisk || disk !== null;
  // Catalog prices assume the default disk; a chosen one moves them.
  const priceOf = (cents: number) =>
    priceWithDisk(kind, cents, disk ?? ROOT_DISK.default);
  const price = chosen ? priceOf(chosen.monthlyCents) : null;
  // This form only shows while the provider is connected.
  const { caps, usage } = providerOf(list, kind)!;
  const overCount = usage.servers + 1 > caps.maxServers;
  const overSpend =
    price !== null ? usage.monthlyCents + price > caps.maxMonthlyCents : false;
  const nameOk = validServerName(name);

  const create = useMutation({
    mutationFn: (req: ProvisionRequest) => provisionApi.provision(workspaceId, req),
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
        <div className="grid max-h-56 grid-cols-3 gap-2 overflow-auto">
          {locations.map((l) => (
            <button
              key={l.location.id}
              type="button"
              aria-pressed={location === l.location.id}
              onClick={() => setPicked(l.location.id)}
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
                      {rootDisk ? (disk ?? ROOT_DISK.default) : t.diskGb} GB
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {t.arch === "arm" ? "Arm" : "x86"} · {t.cpuType}
                    </TableCell>
                    <TableCell className="text-right tabular-nums">
                      {monthlyUsd(priceOf(t.monthlyCents), kind)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        )}
        <p className="text-[13px] text-muted-foreground">{priceNote(kind)}</p>
      </fieldset>

      <div className="grid grid-cols-2 gap-4">
        <div className="flex flex-col gap-4">
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
          {rootDisk ? (
            <Field
              label="Root disk (GB)"
              error={
                diskOk
                  ? null
                  : `Whole GB from ${ROOT_DISK.min} to ${ROOT_DISK.max.toLocaleString("en-US")}`
              }
              hint={`gp3 SSD. Each GB adds ${usd(ROOT_DISK.centsPerGb)} per month.`}
            >
              {(p) => (
                <Input
                  {...p}
                  className="w-32"
                  inputMode="numeric"
                  value={diskText}
                  onChange={(e) => setDiskText(e.target.value)}
                />
              )}
            </Field>
          ) : null}
        </div>
        <div className="flex flex-col gap-1.5">
          <span className="text-sm">SSH access</span>
          <Checkbox
            className="h-8"
            checked={ssh}
            onCheckedChange={setSsh}
            label={`Open port 22 in the ${kind === "aws" ? "security group" : "firewall"}`}
          />
          {ssh ? (
            sshKeys.isPending ? (
              <Skeleton className="h-8" />
            ) : sshKeys.error ? (
              <p className="text-[13px] text-error">{sshKeys.error.message}</p>
            ) : sshKeys.data.length ? (
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
                          {kind === "aws"
                            ? keyRegion(k.id)
                            : k.fingerprint.slice(0, 17)}
                        </span>
                      </>
                    }
                  />
                ))}
              </div>
            ) : (
              <p className="text-[13px] text-muted-foreground">
                {noKeysNote(kind)}
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
          {monthlyUsd(usage.monthlyCents, kind)} of {usd(caps.maxMonthlyCents)} per month
          {price !== null ? ` · adds ${monthlyUsd(price, kind)}` : ""}
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
            disabled={
              !chosen || !location || !nameOk || !diskOk || overCount || overSpend
            }
            onClick={() =>
              create.mutate({
                location: location!,
                serverType: chosen!.name,
                name,
                ssh,
                sshKeyIds: ssh ? keyIds : [],
                ...(rootDisk ? { diskGb: disk! } : {}),
              })
            }
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
