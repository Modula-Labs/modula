import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Check, Plug, World } from "@/components/ui/icons";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { ApiError } from "@/lib/api";
import { cloudflare, type CloudflarePreview, type CloudflareStatus } from "@/lib/integrations";
import { HetznerIntegration } from "./IntegrationsHetzner";

const statusKey = (ws: string) => ["cloudflare", ws];

/** The workspace's Cloudflare connection; shared by settings, backups and domains. */
export function useCloudflare(workspaceId: string) {
  return useQuery({
    queryKey: statusKey(workspaceId),
    queryFn: () => cloudflare.status(workspaceId),
    staleTime: 60_000,
    // Viewers can't read it, and older servers don't have it.
    retry: (n, e) => !(e instanceof ApiError && (e.status === 404 || e.status === 403)) && n < 2,
  });
}

/** Workspace Settings → Integrations. */
export function IntegrationsSection({ workspaceId, isAdmin }: { workspaceId: string; isAdmin: boolean }) {
  const status = useCloudflare(workspaceId);
  const [connecting, setConnecting] = useState(false);

  if (status.error instanceof ApiError && status.error.status === 404) {
    return <Alert tone="warn">This server doesn't support integrations yet.</Alert>;
  }
  return (
    <>
      <div className="divide-y divide-border rounded-lg border border-border">
        {status.isPending ? (
          <div className="p-4">
            <Skeleton className="h-10" />
          </div>
        ) : status.error instanceof ApiError && status.error.status === 403 ? (
          <div className="flex min-h-16 items-center gap-3 px-4 py-3">
            <Plug className="size-5 text-muted-foreground" />
            <div className="text-sm text-muted-foreground">Cloudflare: visible to workspace members and admins.</div>
          </div>
        ) : status.error ? (
          <div className="p-4">
            <Alert>{status.error.message}</Alert>
          </div>
        ) : (
          <CloudflareRow workspaceId={workspaceId} status={status.data} isAdmin={isAdmin} onConnect={() => setConnecting(true)} />
        )}
        <HetznerIntegration workspaceId={workspaceId} canEdit={isAdmin} />
      </div>
      <ConnectCloudflareDialog workspaceId={workspaceId} open={connecting} onOpenChange={setConnecting} replacing={!!status.data?.connected} />
    </>
  );
}

function CloudflareRow({
  workspaceId,
  status,
  isAdmin,
  onConnect,
}: {
  workspaceId: string;
  status: CloudflareStatus | undefined;
  isAdmin: boolean;
  onConnect: () => void;
}) {
  const qc = useQueryClient();
  const disconnect = useMutation({
    mutationFn: () => cloudflare.disconnect(workspaceId),
    onSuccess: (s) => qc.setQueryData(statusKey(workspaceId), s),
  });
  const zones = useQuery({
    queryKey: ["cloudflare-zones", workspaceId],
    queryFn: () => cloudflare.zones(workspaceId),
    enabled: !!status?.connected,
    staleTime: 5 * 60_000,
  });
  const connected = !!status?.connected;
  return (
    <div className="flex flex-col gap-3 px-4 py-3">
      <div className="flex min-h-10 flex-wrap items-center gap-3">
        <Plug className="size-5 text-muted-foreground" />
        <div className="min-w-[14rem] flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-medium">Cloudflare</span>
            {connected ? <Badge tone="success">Connected</Badge> : null}
          </div>
          <div className="text-sm text-muted-foreground">
            {connected ? (
              <>
                {status?.accountName ?? "No account"} · token <span className="font-mono">{status?.tokenHint}</span>
                {status?.connectedBy ? ` · by ${status.connectedBy}` : ""}
              </>
            ) : (
              "Manage DNS for custom domains, proxy them through Cloudflare, and back up to R2."
            )}
          </div>
        </div>
        {isAdmin ? (
          connected ? (
            <div className="ml-auto flex shrink-0 gap-2">
              <Button size="sm" variant="secondary" onClick={onConnect}>
                Replace token
              </Button>
              <Button size="sm" variant="ghost" loading={disconnect.isPending} onClick={() => disconnect.mutate()}>
                Disconnect
              </Button>
            </div>
          ) : (
            <Button className="ml-auto shrink-0" size="sm" variant="secondary" onClick={onConnect}>
              Connect
            </Button>
          )
        ) : null}
      </div>
      {disconnect.error ? <Alert>{disconnect.error.message}</Alert> : null}
      {connected ? (
        zones.isPending ? (
          <Skeleton className="h-6" />
        ) : zones.error ? (
          <Alert tone="warn">{zones.error.message}</Alert>
        ) : (
          <div className="flex flex-wrap gap-1.5 pl-8">
            {zones.data.length === 0 ? <span className="text-sm text-muted-foreground">The token can't see any zones.</span> : null}
            {zones.data.map((z) => (
              <Badge key={z.id} tone={z.status === "active" ? "neutral" : "warn"}>
                {z.name}
                {z.status === "active" ? "" : ` · ${z.status}`}
              </Badge>
            ))}
          </div>
        )
      ) : null}
    </div>
  );
}

function ConnectCloudflareDialog({
  workspaceId,
  open,
  onOpenChange,
  replacing,
}: {
  workspaceId: string;
  open: boolean;
  onOpenChange: (o: boolean) => void;
  replacing: boolean;
}) {
  const qc = useQueryClient();
  const status = useCloudflare(workspaceId);
  const [token, setToken] = useState("");
  const [accountId, setAccountId] = useState("");
  const [preview, setPreview] = useState<CloudflarePreview | null>(null);
  const verify = useMutation({
    mutationFn: () => cloudflare.verify(workspaceId, token),
    onSuccess: (p) => {
      setPreview(p);
      setAccountId(p.accountId ?? p.accounts[0]?.id ?? "");
    },
  });
  const connect = useMutation({
    mutationFn: () => cloudflare.connect(workspaceId, token, accountId || undefined),
    onSuccess: async (s) => {
      qc.setQueryData(statusKey(workspaceId), s);
      await qc.invalidateQueries({ queryKey: ["cloudflare-zones", workspaceId] });
      onOpenChange(false);
    },
  });

  // The token only lives in this dialog; forget it when it closes.
  useEffect(() => {
    if (!open) {
      setToken("");
      setAccountId("");
      setPreview(null);
      verify.reset();
      connect.reset();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const s = status.data;
  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      width={560}
      title={replacing ? "Replace Cloudflare token" : "Connect Cloudflare"}
      description="Modula uses a scoped API token. It's stored encrypted and never shown again."
    >
      <form
        className="flex flex-col gap-5"
        onSubmit={(e) => {
          e.preventDefault();
          if (preview) connect.mutate();
          else if (token.trim()) verify.mutate();
        }}
      >
        <Step n={1} title="Create a token">
          <p className="text-sm text-muted-foreground">
            Open Cloudflare with the permissions pre-filled, optionally narrow it to some zones, and create the token.
          </p>
          <ul className="flex flex-col gap-1 text-sm">
            {s?.permissions.map((p) => (
              <li key={p.name} className="flex gap-2">
                <Check className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
                <span>
                  <span className="font-medium">{p.name}</span>
                  <span className="text-muted-foreground"> to {p.why}</span>
                </span>
              </li>
            ))}
          </ul>
          {s ? (
            <div>
              <a href={s.tokenTemplateUrl} target="_blank" rel="noreferrer" className="text-sm font-medium underline underline-offset-4">
                Create token in Cloudflare
              </a>
            </div>
          ) : null}
        </Step>
        <Step n={2} title="Paste it">
          <Field label="API token">
            {(p) => (
              <Input
                {...p}
                mono
                type="password"
                autoComplete="off"
                value={token}
                onChange={(e) => {
                  setToken(e.target.value);
                  setPreview(null);
                }}
              />
            )}
          </Field>
        </Step>
        <Step n={3} title="Verify">
          {verify.error ? <Alert>{verify.error.message}</Alert> : null}
          {preview ? (
            <div className="flex flex-col gap-3">
              {preview.accounts.length > 1 ? (
                <Field label="Account" hint="R2 buckets are created in this account.">
                  {(p) => (
                    <Select {...p} value={accountId} onChange={(e) => setAccountId(e.target.value)}>
                      {preview.accounts.map((a) => (
                        <option key={a.id} value={a.id}>
                          {a.name || a.id}
                        </option>
                      ))}
                    </Select>
                  )}
                </Field>
              ) : (
                <p className="text-sm">
                  Account: <span className="font-medium">{preview.accounts[0]?.name ?? "none visible (R2 unavailable)"}</span>
                </p>
              )}
              <div>
                <div className="text-sm">
                  {preview.zones.length} zone{preview.zones.length === 1 ? "" : "s"}
                </div>
                <div className="mt-1.5 flex flex-wrap gap-1.5">
                  {preview.zones.map((z) => (
                    <Badge key={z.id}>
                      <World className="size-3" />
                      {z.name}
                    </Badge>
                  ))}
                </div>
              </div>
            </div>
          ) : (
            <div>
              <Button type="submit" variant="secondary" loading={verify.isPending} disabled={!token.trim()}>
                Verify token
              </Button>
            </div>
          )}
        </Step>
        {connect.error ? <Alert>{connect.error.message}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={connect.isPending} disabled={!preview}>
            {replacing ? "Replace token" : "Connect"}
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function Step({ n, title, children }: { n: number; title: string; children: ReactNode }) {
  return (
    <div className="flex gap-3">
      <span className="flex size-6 shrink-0 items-center justify-center rounded-full border border-border text-xs font-medium text-muted-foreground">{n}</span>
      <div className="flex min-w-0 flex-1 flex-col gap-2">
        <div className="text-sm font-semibold">{title}</div>
        {children}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------- R2

/** "Use Cloudflare R2" next to "Add storage", shown once Cloudflare is connected. */
export function UseR2Button({ workspaceId }: { workspaceId: string }) {
  const status = useCloudflare(workspaceId);
  const [open, setOpen] = useState(false);
  if (!status.data?.connected || !status.data.accountId) return null;
  return (
    <>
      <Button variant="secondary" size="sm" onClick={() => setOpen(true)}>
        Use Cloudflare R2
      </Button>
      <R2Dialog workspaceId={workspaceId} open={open} onOpenChange={setOpen} />
    </>
  );
}

function R2Dialog({ workspaceId, open, onOpenChange }: { workspaceId: string; open: boolean; onOpenChange: (o: boolean) => void }) {
  const qc = useQueryClient();
  const info = useQuery({ queryKey: ["cloudflare-r2", workspaceId], queryFn: () => cloudflare.r2(workspaceId), enabled: open });
  const blank = { name: "Cloudflare R2", bucket: "modula-backups", prefix: "", accessKeyId: "", secretAccessKey: "" };
  const [f, setF] = useState(blank);
  const set = (k: keyof typeof blank) => (e: { target: { value: string } }) => setF((x) => ({ ...x, [k]: e.target.value }));
  const create = useMutation({
    mutationFn: () =>
      cloudflare.createR2Target(workspaceId, {
        name: f.name.trim(),
        bucket: f.bucket.trim(),
        prefix: f.prefix.trim() || undefined,
        accessKeyId: f.accessKeyId.trim(),
        secretAccessKey: f.secretAccessKey,
      }),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["backup-targets", workspaceId] });
      onOpenChange(false);
    },
  });
  useEffect(() => {
    if (!open) {
      setF(blank);
      create.reset();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);
  const valid = f.name.trim() && f.bucket.trim() && f.accessKeyId.trim() && f.secretAccessKey;

  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      width={520}
      title="Use Cloudflare R2"
      description="Modula creates the bucket in your Cloudflare account and adds it as backup storage."
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) create.mutate();
        }}
      >
        {info.error ? <Alert>{info.error.message}</Alert> : null}
        {info.data && !info.data.canCreateBuckets ? (
          <Alert tone="warn">
            The token can't create R2 buckets. Add the Workers R2 Storage · Edit permission, or create the bucket in the dashboard first.
          </Alert>
        ) : null}
        <div className="grid grid-cols-2 gap-3">
          <Field label="Name">{(p) => <Input {...p} value={f.name} onChange={set("name")} />}</Field>
          <Field label="Bucket" hint="Created if it doesn't exist.">
            {(p) => <Input {...p} mono value={f.bucket} onChange={set("bucket")} />}
          </Field>
        </div>
        <Field label="Endpoint">{(p) => <Input {...p} mono readOnly value={info.data?.endpoint ?? ""} placeholder="Loading…" />}</Field>
        <p className="text-sm text-muted-foreground">
          Cloudflare's API can't create S3 credentials.{" "}
          {info.data ? (
            <a href={info.data.apiTokensUrl} target="_blank" rel="noreferrer" className="font-medium text-foreground underline underline-offset-4">
              Create an R2 API token
            </a>
          ) : (
            "Create an R2 API token"
          )}{" "}
          with Object Read & Write on this bucket, then paste its S3 credentials.
        </p>
        <Field label="Access key ID">{(p) => <Input {...p} mono autoComplete="off" value={f.accessKeyId} onChange={set("accessKeyId")} />}</Field>
        <Field label="Secret access key" hint="Stored encrypted. It's never shown again.">
          {(p) => <Input {...p} mono type="password" autoComplete="new-password" value={f.secretAccessKey} onChange={set("secretAccessKey")} />}
        </Field>
        {create.error ? <Alert>{create.error.message}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending} disabled={!valid}>
            Add R2 storage
          </Button>
        </div>
      </form>
    </Modal>
  );
}
