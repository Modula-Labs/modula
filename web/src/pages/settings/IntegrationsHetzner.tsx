import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/Button";
import { Card, CardContent } from "@/components/ui/Card";
import { Server } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { provisionApi } from "@/lib/provision";
import { ProviderLimits } from "./ProviderLimits";

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

  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["hetzner", workspaceId] });
    qc.invalidateQueries({ queryKey: ["provisioned", workspaceId] });
  };
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
          <ProviderLimits
            workspaceId={workspaceId}
            kind="hetzner"
            status={s}
            canEdit={canEdit}
            usageNote="incl. VAT"
            onChanged={refresh}
          />
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
