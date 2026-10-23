import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Trash } from "@/components/ui/icons";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, StatusDot } from "@/components/ui/misc";
import { ApiError } from "@/lib/api";
import { flagEmoji } from "@/lib/flags";
import { euros, provisionApi, type ProvisionStatus, type ProvisionedServer } from "@/lib/provision";

const LABEL: Record<ProvisionStatus, string> = {
  creating: "Creating",
  booting: "Booting",
  joining: "Joining",
  ready: "Ready",
  failed: "Failed",
  deleting: "Deleting",
};
const TONE: Record<ProvisionStatus, "success" | "warn" | "danger" | "neutral"> = {
  creating: "warn",
  booting: "warn",
  joining: "warn",
  ready: "success",
  failed: "danger",
  deleting: "neutral",
};

/** Servers this workspace created at Hetzner, with live status and delete. */
export function ProvisionedServers({ workspaceId }: { workspaceId: string | undefined }) {
  const list = useQuery({
    queryKey: ["provisioned", workspaceId],
    queryFn: () => provisionApi.servers(workspaceId!),
    enabled: !!workspaceId,
    refetchInterval: (q) => (q.state.data?.servers.some((s) => !["ready", "failed"].includes(s.status)) ? 3000 : 30_000),
  });
  const [deleting, setDeleting] = useState<ProvisionedServer | null>(null);
  const servers = list.data?.servers ?? [];
  if (!list.data?.hetznerConnected && servers.length === 0) return null;

  return (
    <section className="mt-10">
      <div className="flex items-baseline justify-between gap-4">
        <h2 className="text-base font-semibold">Created at Hetzner</h2>
        {list.data ? (
          <span className="text-xs text-muted-foreground">
            {list.data.usage.servers} of {list.data.caps.maxServers} servers · {euros(list.data.usage.monthlyCents)} of{" "}
            {euros(list.data.caps.maxMonthlyCents)} per month
          </span>
        ) : null}
      </div>
      {list.error ? <Alert>{list.error.message}</Alert> : null}
      {servers.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">None yet. Use Add server → Hetzner Cloud.</p>
      ) : (
        <div className="mt-3 divide-y divide-border rounded-lg border border-border">
          {servers.map((s) => (
            <div key={s.id} className="flex min-h-14 items-center gap-4 px-4 py-2.5">
              <span aria-hidden className="text-base">
                {flagEmoji(s.location?.countryCode)}
              </span>
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  <span className="truncate font-medium">{s.name}</span>
                  <Badge>{s.serverType}</Badge>
                  {s.sshEnabled ? <Badge>ssh</Badge> : null}
                </div>
                <div className="truncate text-xs text-muted-foreground">
                  {[s.location?.city, s.ipv4, `${euros(s.monthlyPriceCents)}/mo`].filter(Boolean).join(" · ")}
                  {s.status === "failed" && s.error ? <span className="text-error"> · {s.error}</span> : null}
                </div>
              </div>
              <StatusDot tone={TONE[s.status]}>{LABEL[s.status]}</StatusDot>
              <Button variant="ghost" size="icon" aria-label={`Delete ${s.name}`} onClick={() => setDeleting(s)}>
                <Trash />
              </Button>
            </div>
          ))}
        </div>
      )}
      <DeleteServerDialog server={deleting} workspaceId={workspaceId} onClose={() => setDeleting(null)} />
    </section>
  );
}

function DeleteServerDialog({
  server,
  workspaceId,
  onClose,
}: {
  server: ProvisionedServer | null;
  workspaceId: string | undefined;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const remove = useMutation({
    mutationFn: (force: boolean) => provisionApi.deleteServer(server!.id, force),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["provisioned", workspaceId] });
      qc.invalidateQueries({ queryKey: ["nodes"] });
      close();
    },
  });
  const close = () => {
    remove.reset();
    onClose();
  };
  // The server refuses while services run there; offer to delete anyway.
  const busy = remove.error instanceof ApiError && remove.error.status === 409;

  return (
    <Modal
      open={!!server}
      onOpenChange={(o) => (o ? null : close())}
      title={`Delete ${server?.name ?? "server"}?`}
      description="The server is deleted at Hetzner with everything on its disk, and removed from this instance. Billing stops."
    >
      <div className="flex flex-col gap-4">
        {remove.error ? <Alert tone={busy ? "warn" : "danger"}>{remove.error.message}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={close}>
            Cancel
          </Button>
          {busy ? (
            <Button variant="danger" loading={remove.isPending} onClick={() => remove.mutate(true)}>
              Delete anyway
            </Button>
          ) : (
            <Button variant="danger" loading={remove.isPending} onClick={() => remove.mutate(false)}>
              Delete server
            </Button>
          )}
        </div>
      </div>
    </Modal>
  );
}
