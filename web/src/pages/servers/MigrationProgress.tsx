import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useState } from "react";
import { Button, Spinner } from "@/components/ui/Button";
import { Check } from "@/components/ui/icons";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { flagEmoji } from "@/lib/flags";
import { formatBytes } from "@/lib/format";
import {
  migrateApi,
  migrationActive,
  type InstanceMove,
  type ProgressStep,
  PROMOTE_LABELS,
} from "@/lib/migrate";
import { provisionApi } from "@/lib/provision";

/** A step list: done, running (spinner) or failed, with an optional detail. */
export function StepList({ steps, compact }: { steps: ProgressStep[]; compact?: boolean }) {
  return (
    <ol className={clsx("flex flex-col", compact ? "gap-1.5" : "gap-2.5")} aria-label="Progress">
      {steps.map((s, i) => (
        <li
          key={i}
          className="flex items-start gap-3 text-sm"
          aria-current={s.status === "running" ? "step" : undefined}
        >
          <span
            className={clsx(
              "mt-px flex size-5 shrink-0 items-center justify-center rounded-full border",
              s.status === "done"
                ? "border-success bg-badge-green-bg text-badge-green-fg"
                : s.status === "failed"
                  ? "border-error text-error"
                  : "border-border text-muted-foreground",
            )}
          >
            {s.status === "done" ? (
              <Check className="size-3" />
            ) : s.status === "running" ? (
              <Spinner className="size-3" />
            ) : (
              <span aria-hidden className="text-xs leading-none">
                ×
              </span>
            )}
          </span>
          <span className="min-w-0">
            <span className={clsx(s.status === "failed" && "text-error")}>{s.label}</span>
            {s.detail ? (
              <span className="ml-2 text-[13px] text-muted-foreground">{s.detail}</span>
            ) : null}
          </span>
        </li>
      ))}
    </ol>
  );
}

const MOVE_TONE = {
  pending: "neutral",
  running: "info",
  failed: "danger",
  aborting: "warn",
  aborted: "neutral",
  done: "success",
} as const;

function MoveRow({ move }: { move: InstanceMove }) {
  return (
    <li className="flex flex-col gap-2 rounded-lg border border-border px-3 py-2.5">
      <div className="flex items-center gap-2 text-sm">
        <span className="font-medium">{move.service}</span>
        <span className="text-muted-foreground">
          {move.project} · {move.environment}
        </span>
        {move.kind === "replicas" ? <Badge>replicas</Badge> : null}
        <span className="ml-auto">
          <Badge tone={MOVE_TONE[move.status]}>{move.status}</Badge>
        </span>
      </div>
      {move.steps.length ? <StepList steps={move.steps} compact /> : null}
      {move.error ? <p className="text-[13px] text-error">{move.error}</p> : null}
    </li>
  );
}

/**
 * A server migration, polled live: its steps, each service's move, abort
 * while it runs (or after it failed), and deleting the old server once done.
 */
export function MigrationProgress({
  migrationId,
  onDone,
}: {
  migrationId: string;
  onDone?: () => void;
}) {
  const qc = useQueryClient();
  const m = useQuery({
    queryKey: ["server-migration", migrationId],
    queryFn: () => migrateApi.migration(migrationId),
    refetchInterval: (q) => (migrationActive(q.state.data?.status) ? 2000 : false),
  });
  const [confirmAbort, setConfirmAbort] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["server-migration", migrationId] });
    qc.invalidateQueries({ queryKey: ["provisioned"] });
    qc.invalidateQueries({ queryKey: ["nodes"] });
  };
  const abort = useMutation({
    mutationFn: () => migrateApi.abort(migrationId),
    onSuccess: () => {
      setConfirmAbort(false);
      refresh();
    },
  });
  const del = useMutation({
    mutationFn: () => provisionApi.deleteServer(m.data!.sourceServerId!),
    onSuccess: () => {
      setConfirmDelete(false);
      refresh();
    },
  });

  if (m.isPending) return <Skeleton className="h-40" />;
  if (m.error) return <Alert>{m.error.message}</Alert>;
  const s = m.data;
  const canAbort =
    (s.status === "running" || s.status === "failed") && s.promotePhase !== "switch";
  const deletable = s.status === "done" && s.sourcePendingDeletion && !!s.sourceServerId;
  const moved = s.moves.filter((x) => x.status === "done").length;

  return (
    <div className="flex flex-col gap-5">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border border-border px-4 py-3 text-sm">
        <span className="font-mono">{s.sourceName}</span>
        <span aria-hidden className="text-muted-foreground">
          →
        </span>
        <span aria-hidden>{flagEmoji(s.targetLocation?.countryCode)}</span>
        <span className="font-mono">{s.targetName ?? "new server"}</span>
        <span className="text-muted-foreground">
          {s.targetLocation?.city} · {s.targetServerType}
        </span>
        <span className="ml-auto">
          <Badge
            tone={
              s.status === "done"
                ? "success"
                : s.status === "failed"
                  ? "danger"
                  : s.status === "aborted"
                    ? "neutral"
                    : "info"
            }
          >
            {s.status}
          </Badge>
        </span>
      </div>

      <StepList steps={s.steps} />

      {s.main && s.step === "promote" && s.status === "running" ? (
        <Alert tone="info" title="Moving the dashboard">
          {s.promotePhase ? PROMOTE_LABELS[s.promotePhase] : "Preparing the handover"}.
          {s.promotePhase === "switch"
            ? " This can't be aborted any more."
            : " Changes are refused until it finishes; apps keep running."}
        </Alert>
      ) : null}
      {s.main && s.newPublicUrl ? (
        <p className="text-sm">
          <span className="text-muted-foreground">The dashboard will be at</span>{" "}
          <a className="font-mono underline" href={s.newPublicUrl}>
            {s.newPublicUrl}
          </a>
        </p>
      ) : null}

      {s.moves.length ? (
        <section className="flex flex-col gap-2">
          <h3 className="text-sm font-bold">
            Services{" "}
            <span className="font-normal text-muted-foreground">
              {moved} of {s.moves.length} moved
            </span>
          </h3>
          <ul className="flex max-h-80 flex-col gap-2 overflow-auto">
            {s.moves.map((mv) => (
              <MoveRow key={mv.id} move={mv} />
            ))}
          </ul>
        </section>
      ) : null}

      {s.status === "failed" ? (
        <Alert title="The migration stopped">
          {s.error ?? "Unknown error."} Services not moved yet are untouched;
          a service that was stopped for its copy stays stopped until you
          abort, which starts everything again on {s.sourceName}.
        </Alert>
      ) : null}
      {s.status === "aborted" ? (
        <Alert tone="info" title="Aborted">
          Everything runs on {s.sourceName} again.
          {s.targetServerId
            ? ` ${s.targetName ?? "The new server"} was kept; delete it from Servers if you don't need it.`
            : ""}
        </Alert>
      ) : null}

      {s.status === "done" && s.tcpProxies.length ? (
        <Alert tone="warn" title="New TCP proxy addresses">
          Clients connecting over TCP must use the new addresses:
          <ul className="mt-1.5 font-mono text-[13px]">
            {s.tcpProxies.map((p) => (
              <li key={`${p.service}-${p.publicPort}`}>
                {p.service}: {p.host ?? "new server"}:{p.publicPort} → :{p.targetPort}
              </li>
            ))}
          </ul>
        </Alert>
      ) : null}
      {deletable ? (
        <Alert tone="info" title={`${s.sourceName} is empty`}>
          Nothing runs on it any more, but it is still billed. Delete it once
          you've checked everything works on {s.targetName}.
        </Alert>
      ) : null}
      {abort.error ? <Alert>{abort.error.message}</Alert> : null}
      {del.isSuccess ? (
        <Alert tone="success" title="Deleted">
          {s.sourceName} was deleted at Hetzner.
        </Alert>
      ) : null}

      <div className="flex justify-end gap-2">
        {canAbort ? (
          <Button variant="danger-outline" onClick={() => setConfirmAbort(true)}>
            Abort
          </Button>
        ) : null}
        {deletable && !del.isSuccess ? (
          <Button variant="danger" onClick={() => setConfirmDelete(true)}>
            Delete old server
          </Button>
        ) : null}
        {onDone ? (
          <Button variant="secondary" onClick={onDone}>
            {migrationActive(s.status) ? "Close — keep going in the background" : "Done"}
          </Button>
        ) : null}
      </div>

      <Modal
        open={confirmAbort}
        onOpenChange={setConfirmAbort}
        title="Abort the migration?"
        description={`Every service moved so far goes back to ${s.sourceName} and starts there again, with the data it had there. Anything written on the new server since it moved is not copied back.`}
      >
        {abort.error ? <Alert>{abort.error.message}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => setConfirmAbort(false)}>
            Keep going
          </Button>
          <Button variant="danger" loading={abort.isPending} onClick={() => abort.mutate()}>
            Abort and move back
          </Button>
        </div>
      </Modal>
      <Modal
        open={confirmDelete}
        onOpenChange={setConfirmDelete}
        title={`Delete ${s.sourceName}?`}
        description="The server is deleted at Hetzner with its disks, including the old copies of the volumes. This can't be undone."
      >
        {del.error ? <Alert>{del.error.message}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => setConfirmDelete(false)}>
            Cancel
          </Button>
          <Button variant="danger" loading={del.isPending} onClick={() => del.mutate()}>
            Delete server
          </Button>
        </div>
      </Modal>
    </div>
  );
}

/** One service's move to another server, polled live, with abort. */
export function InstanceMoveProgress({ moveId, onDone }: { moveId: string; onDone?: () => void }) {
  const qc = useQueryClient();
  const m = useQuery({
    queryKey: ["instance-move", moveId],
    queryFn: () => migrateApi.move(moveId),
    refetchInterval: (q) => (migrationActive(q.state.data?.status) ? 2000 : false),
  });
  const abort = useMutation({
    mutationFn: () => migrateApi.abortMove(moveId),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["instance-move", moveId] }),
  });
  if (m.isPending) return <Skeleton className="h-24" />;
  if (m.error) return <Alert>{m.error.message}</Alert>;
  const mv = m.data;
  return (
    <div className="flex flex-col gap-4">
      <p className="text-sm">
        {mv.service} from <span className="font-mono">{mv.fromName ?? "—"}</span> to{" "}
        <span className="font-mono">{mv.toName ?? "—"}</span>
        {mv.copiedBytes > 0 ? (
          <span className="text-muted-foreground"> · {formatBytes(mv.copiedBytes)} copied</span>
        ) : null}
      </p>
      {mv.steps.length ? <StepList steps={mv.steps} /> : <Skeleton className="h-8" />}
      {mv.status === "failed" ? (
        <Alert title="The move stopped">
          {mv.error ?? "Unknown error."} Abort to start it again on {mv.fromName ?? "its old server"}.
        </Alert>
      ) : null}
      {abort.error ? <Alert>{abort.error.message}</Alert> : null}
      <div className="flex justify-end gap-2">
        {(mv.status === "running" || mv.status === "failed") && !mv.migrationId ? (
          <Button variant="danger-outline" loading={abort.isPending} onClick={() => abort.mutate()}>
            Abort and move back
          </Button>
        ) : null}
        {onDone ? (
          <Button variant="secondary" onClick={onDone}>
            {migrationActive(mv.status) ? "Close — keep going in the background" : "Done"}
          </Button>
        ) : null}
      </div>
    </div>
  );
}
