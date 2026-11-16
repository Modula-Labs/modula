import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/Button";
import { Card, CardContent } from "@/components/ui/Card";
import { CopyButton } from "@/components/ui/CopyButton";
import { Server } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { provisionApi } from "@/lib/provision";
import { ProviderLimits } from "./ProviderLimits";

/** Workspace Settings → Integrations → AWS (EC2). */
export function AwsIntegration({
  workspaceId,
  canEdit,
}: {
  workspaceId: string;
  canEdit: boolean;
}) {
  const qc = useQueryClient();
  const status = useQuery({
    queryKey: ["aws", workspaceId],
    queryFn: () => provisionApi.aws(workspaceId),
  });
  const [connecting, setConnecting] = useState(false);

  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["aws", workspaceId] });
    qc.invalidateQueries({ queryKey: ["provisioned", workspaceId] });
    qc.invalidateQueries({ queryKey: ["aws-catalog", workspaceId] });
    qc.invalidateQueries({ queryKey: ["aws-ssh-keys", workspaceId] });
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
              <span className="font-bold">AWS EC2</span>
              {s.connected ? (
                <Badge tone="success">Connected</Badge>
              ) : (
                <Badge>Not connected</Badge>
              )}
            </div>
            <p className="mt-0.5 text-sm text-muted-foreground">
              {s.connected
                ? `Connected ${s.connectedAt ? relativeTime(s.connectedAt) : ""}${s.connectedBy ? ` by ${s.connectedBy}` : ""}. Servers are only created when you add one.`
                : "Create worker servers on EC2 in any region your account has enabled from the Servers page. Each gets an Elastic IP."}
            </p>
          </div>
          {canEdit ? (
            <Button
              className="ml-auto shrink-0"
              variant="outline"
              onClick={() => setConnecting(true)}
            >
              {s.connected ? "Replace access key" : "Connect"}
            </Button>
          ) : null}
        </div>

        {s.connected ? (
          <ProviderLimits
            workspaceId={workspaceId}
            kind="aws"
            status={s}
            canEdit={canEdit}
            usageNote="compute, disk and Elastic IPs; excludes data transfer"
            onChanged={refresh}
          />
        ) : null}

        <ConnectDialog
          open={connecting}
          onOpenChange={setConnecting}
          workspaceId={workspaceId}
          keyUrl={s.keyUrl}
          policy={JSON.stringify(s.iamPolicy, null, 2)}
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
  keyUrl,
  policy,
  replacing,
  onConnected,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  workspaceId: string;
  keyUrl: string;
  policy: string;
  replacing: boolean;
  onConnected: () => void;
}) {
  const [accessKeyId, setAccessKeyId] = useState("");
  const [secret, setSecret] = useState("");
  const connect = useMutation({
    mutationFn: () =>
      provisionApi.connectAws(workspaceId, {
        accessKeyId: accessKeyId.trim(),
        secretAccessKey: secret.trim(),
      }),
    onSuccess: () => {
      onConnected();
      close(false);
    },
  });
  // The key only lives in this dialog; forget it when it closes.
  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) {
      setAccessKeyId("");
      setSecret("");
      connect.reset();
    }
  };
  const ready = !!accessKeyId.trim() && !!secret.trim();
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (ready) connect.mutate();
  };

  return (
    <Modal
      open={open}
      onOpenChange={close}
      width={600}
      title={replacing ? "Replace AWS access key" : "Connect AWS EC2"}
      description="The key is verified, encrypted with this workspace's key, and never shown again."
    >
      <form onSubmit={submit} className="flex flex-col gap-4">
        {connect.error ? <Alert>{connect.error.message}</Alert> : null}
        <ol className="flex list-decimal flex-col gap-1.5 pl-5 text-sm text-muted-foreground">
          <li>
            Open{" "}
            <a
              href={keyUrl}
              target="_blank"
              rel="noreferrer noopener"
              className="text-foreground underline underline-offset-2"
            >
              IAM users
            </a>{" "}
            in the AWS console and create a user for Modula. It needs no console
            access.
          </li>
          <li>
            Give it this policy (Add permissions → Create inline policy → JSON).
            It allows only what Modula needs to create and manage servers.
          </li>
          <li>
            Open the user's Security credentials, create an access key for an
            application running outside AWS, and paste it below.
          </li>
        </ol>
        <div className="relative rounded-lg border border-border bg-muted">
          <pre
            tabIndex={0}
            aria-label="IAM policy"
            className="max-h-44 overflow-auto p-3 pr-11 font-mono text-xs leading-5"
          >
            {policy}
          </pre>
          <div className="absolute top-1.5 right-1.5">
            <CopyButton value={policy} label="Copy policy" />
          </div>
        </div>
        <Field label="Access key ID">
          {(p) => (
            <Input
              {...p}
              mono
              autoComplete="off"
              spellCheck={false}
              value={accessKeyId}
              onChange={(e) => setAccessKeyId(e.target.value)}
            />
          )}
        </Field>
        <Field label="Secret access key">
          {(p) => (
            <Input
              {...p}
              mono
              type="password"
              autoComplete="off"
              spellCheck={false}
              value={secret}
              onChange={(e) => setSecret(e.target.value)}
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
            disabled={!ready}
          >
            Verify and connect
          </Button>
        </div>
      </form>
    </Modal>
  );
}
