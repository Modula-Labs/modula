import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState, type FormEvent } from "react";
import { Button } from "@/components/ui/Button";
import { Card, CardContent } from "@/components/ui/Card";
import { Server } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { euros, provisionApi } from "@/lib/provision";

/** Workspace Settings → Integrations → Hetzner Cloud. */
export function HetznerIntegration({
  workspaceId,
  canEdit,
}: {
  workspaceId: string;
  canEdit: boolean;
}) {
  const qc = useQueryClient();
  const status = useQuery({
    queryKey: ["hetzner", workspaceId],
    queryFn: () => provisionApi.hetzner(workspaceId),
  });
  const [connecting, setConnecting] = useState(false);
  const [maxServers, setMaxServers] = useState("");
  const [maxEuros, setMaxEuros] = useState("");
  useEffect(() => {
    if (!status.data) return;
    setMaxServers(String(status.data.caps.maxServers));
    setMaxEuros(String(status.data.caps.maxMonthlyCents / 100));
  }, [status.data]);

  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["hetzner", workspaceId] });
    qc.invalidateQueries({ queryKey: ["provisioned", workspaceId] });
  };
  const saveCaps = useMutation({
    mutationFn: () =>
      provisionApi.updateHetznerCaps(workspaceId, {
        maxServers: Number(maxServers),
        maxMonthlyCents: Math.round(Number(maxEuros) * 100),
      }),
    onSuccess: refresh,
  });
  const disconnect = useMutation({
    mutationFn: () => provisionApi.disconnectHetzner(workspaceId),
    onSuccess: refresh,
  });

  if (status.isPending)
    return (
      <Card size="sm">
        <CardContent>
          <Skeleton className="h-10" />
        </CardContent>
      </Card>
    );
  if (status.error)
    return (
      <Card size="sm">
        <CardContent>
          <Alert>{status.error.message}</Alert>
        </CardContent>
      </Card>
    );
  const s = status.data;
  const capsValid =
    /^\d+$/.test(maxServers) &&
    Number(maxEuros) >= 0 &&
    maxEuros.trim() !== "" &&
    !Number.isNaN(Number(maxEuros));
  const capsDirty =
    Number(maxServers) !== s.caps.maxServers ||
    Math.round(Number(maxEuros) * 100) !== s.caps.maxMonthlyCents;

  return (
    <Card size="sm">
      <CardContent>
        <div className="flex flex-wrap items-start gap-3">
          <Server className="mt-0.5 size-5 text-muted-foreground" />
          <div className="min-w-[14rem] flex-1">
            <div className="flex flex-wrap items-center gap-2">
              <span className="font-bold">Hetzner Cloud</span>
              {s.connected ? (
                <Badge tone="success">Connected</Badge>
              ) : (
                <Badge>Not connected</Badge>
              )}
            </div>
            <p className="mt-0.5 text-sm text-muted-foreground">
              {s.connected
                ? `Connected ${s.connectedAt ? relativeTime(s.connectedAt) : ""}${s.connectedBy ? ` by ${s.connectedBy}` : ""}. Servers are only created when you add one.`
                : "Create worker servers in Falkenstein, Nuremberg, Helsinki, Ashburn, Hillsboro or Singapore from the Servers page."}
            </p>
          </div>
          {canEdit ? (
            <Button
              className="ml-auto shrink-0"
              variant="outline"
              onClick={() => setConnecting(true)}
            >
              {s.connected ? "Replace token" : "Connect"}
            </Button>
          ) : null}
        </div>

        {s.connected ? (
          <div className="mt-4 flex flex-col gap-3 border-t border-border pt-4">
            {(saveCaps.error ?? disconnect.error) ? (
              <Alert>{(saveCaps.error ?? disconnect.error)?.message}</Alert>
            ) : null}
            <div className="flex flex-wrap items-end gap-3">
              <Field label="Max servers">
                {(p) => (
                  <Input
                    {...p}
                    className="w-28"
                    inputMode="numeric"
                    disabled={!canEdit}
                    value={maxServers}
                    onChange={(e) => setMaxServers(e.target.value)}
                  />
                )}
              </Field>
              <Field label="Monthly limit (€)">
                {(p) => (
                  <Input
                    {...p}
                    className="w-32"
                    inputMode="decimal"
                    disabled={!canEdit}
                    value={maxEuros}
                    onChange={(e) => setMaxEuros(e.target.value)}
                  />
                )}
              </Field>
              {canEdit ? (
                <Button
                  variant="secondary"
                  loading={saveCaps.isPending}
                  disabled={!capsValid || !capsDirty}
                  onClick={() => saveCaps.mutate()}
                >
                  Save limits
                </Button>
              ) : null}
            </div>
            <p className="text-[13px] text-muted-foreground">
              In use: {s.usage.servers} server{s.usage.servers === 1 ? "" : "s"}
              , {euros(s.usage.monthlyCents)} per month (incl. VAT).
            </p>
            {canEdit ? (
              <div>
                <Button
                  variant="danger-outline"
                  loading={disconnect.isPending}
                  onClick={() => disconnect.mutate()}
                >
                  Disconnect
                </Button>
              </div>
            ) : null}
          </div>
        ) : null}

        <ConnectDialog
          open={connecting}
          onOpenChange={setConnecting}
          workspaceId={workspaceId}
          tokenUrl={s.tokenUrl}
          replacing={s.connected}
          onConnected={refresh}
        />
      </CardContent>
    </Card>
  );
}

function ConnectDialog({
  open,
  onOpenChange,
  workspaceId,
  tokenUrl,
  replacing,
  onConnected,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  workspaceId: string;
  tokenUrl: string;
  replacing: boolean;
  onConnected: () => void;
}) {
  const [token, setToken] = useState("");
  const connect = useMutation({
    mutationFn: () => provisionApi.connectHetzner(workspaceId, token.trim()),
    onSuccess: () => {
      onConnected();
      close(false);
    },
  });
  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) {
      setToken("");
      connect.reset();
    }
  };
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (token.trim()) connect.mutate();
  };

  return (
    <Modal
      open={open}
      onOpenChange={close}
      width={520}
      title={replacing ? "Replace Hetzner token" : "Connect Hetzner Cloud"}
      description="The token is verified, encrypted with this workspace's key, and never shown again."
    >
      <form onSubmit={submit} className="flex flex-col gap-4">
        {connect.error ? <Alert>{connect.error.message}</Alert> : null}
        <ol className="flex list-decimal flex-col gap-1.5 pl-5 text-sm text-muted-foreground">
          <li>
            Open the{" "}
            <a
              href={tokenUrl}
              target="_blank"
              rel="noreferrer noopener"
              className="text-foreground underline underline-offset-2"
            >
              Hetzner Cloud console
            </a>{" "}
            and pick the project for your servers.
          </li>
          <li>Go to Security → API tokens → Generate API token.</li>
          <li>
            Choose{" "}
            <span className="font-bold text-foreground">Read &amp; Write</span>{" "}
            and paste the token here.
          </li>
        </ol>
        <Field label="API token">
          {(p) => (
            <Input
              {...p}
              mono
              type="password"
              autoComplete="off"
              spellCheck={false}
              value={token}
              onChange={(e) => setToken(e.target.value)}
            />
          )}
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => close(false)}>
            Cancel
          </Button>
          <Button
            type="submit"
            variant="primary"
            loading={connect.isPending}
            disabled={!token.trim()}
          >
            Verify and connect
          </Button>
        </div>
      </form>
    </Modal>
  );
}
