import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Skeleton } from "@/components/ui/misc";
import { api, type Instance, type Replication, type ReplicationPeer } from "@/lib/api";
import { formatLag, formatLagSeconds } from "@/lib/pitr";

/** Read replicas of a Postgres service: add one, see lag, or promote a replica. */
export function ReplicationSection({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const qc = useQueryClient();
  const key = ["replication", instance.id];
  const info = useQuery({ queryKey: key, queryFn: () => api.replication(instance.id), refetchInterval: 10000 });
  const [adding, setAdding] = useState(false);
  const [promoting, setPromoting] = useState(false);

  if (info.isPending) return <Skeleton className="h-24" />;
  if (info.error) return <Alert tone="danger">{info.error.message}</Alert>;
  const r = info.data;

  return (
    <section className="flex flex-col gap-4">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h3 className="text-base leading-6 font-bold">Replication</h3>
          <p className="mt-1 text-sm text-muted-foreground">
            {r.role === "replica"
              ? `A read replica of ${r.primary?.serviceName ?? "its primary"}. Its DATABASE_URL is read-only.`
              : "Read replicas follow this database and serve read-only queries. The primary gets DATABASE_REPLICA_URL on its next deploy."}
          </p>
        </div>
        {canEdit && r.role !== "replica" ? (
          <Button variant="secondary" onClick={() => setAdding(true)}>
            Add read replica
          </Button>
        ) : null}
        {canEdit && r.role === "replica" ? (
          <Button variant="secondary" onClick={() => setPromoting(true)}>
            Promote
          </Button>
        ) : null}
      </div>

      {r.role === "replica" ? <ReplicaStatus r={r} instance={instance} /> : null}
      {r.role === "primary" ? <Replicas r={r} instance={instance} /> : null}

      <AddReplicaDialog
        instance={instance}
        open={adding}
        onClose={() => setAdding(false)}
        onDone={() => {
          void qc.invalidateQueries({ queryKey: key });
          void qc.invalidateQueries({ queryKey: ["project", instance.projectId] });
        }}
      />
      <PromoteDialog
        instance={instance}
        open={promoting}
        onClose={() => setPromoting(false)}
        onDone={(next) => {
          qc.setQueryData(key, next);
          void qc.invalidateQueries({ queryKey: ["project", instance.projectId] });
        }}
      />
    </section>
  );
}

function PeerLink({ peer, instance }: { peer: ReplicationPeer; instance: Instance }) {
  return (
    <Link
      to="/projects/$projectId"
      params={{ projectId: instance.projectId }}
      search={{ env: instance.environmentId, service: peer.serviceId }}
      className="underline"
    >
      {peer.serviceName}
    </Link>
  );
}

function ReplicaStatus({ r, instance }: { r: Replication; instance: Instance }) {
  const s = r.status?.role === "replica" ? r.status : null;
  return (
    <div className="flex flex-col gap-1 rounded-xl border border-border px-4 py-3 text-sm">
      <div>
        Read replica of {r.primary ? <PeerLink peer={r.primary} instance={instance} /> : "its primary"}
      </div>
      <div className="text-[13px] text-muted-foreground">
        {s ? (s.caughtUp ? "Caught up" : `Replay lag ${formatLagSeconds(s.replayLagSeconds)}`) : "Status is unavailable while the database is not running."}
      </div>
    </div>
  );
}

function Replicas({ r, instance }: { r: Replication; instance: Instance }) {
  const standbys = r.status?.role === "primary" ? r.status.standbys : [];
  return (
    <div className="divide-y divide-border overflow-hidden rounded-xl border border-border">
      {r.replicas.map((p, i) => {
        // pg_stat_replication names standbys by application_name, so match by position when names differ.
        const lag = standbys.find((s) => s.name === p.serviceName) ?? (standbys.length === r.replicas.length ? standbys[i] : undefined);
        return (
          <div key={p.serviceId} className="flex min-h-12 items-center gap-3 px-3 py-2">
            <div className="min-w-0 flex-1 text-sm">
              <PeerLink peer={p} instance={instance} />
            </div>
            <span className="text-[13px] text-muted-foreground">
              {lag ? `${lag.state ?? "unknown"} · ${formatLag(lag.replayLagSeconds, lag.lagBytes)}` : "Lag unavailable"}
            </span>
          </div>
        );
      })}
    </div>
  );
}

function AddReplicaDialog({ instance, open, onClose, onDone }: { instance: Instance; open: boolean; onClose: () => void; onDone: () => void }) {
  const [name, setName] = useState("");
  const add = useMutation({
    mutationFn: () => api.addReplica(instance.id, name.trim() ? { name: name.trim() } : {}),
    onSuccess: () => {
      onDone();
      close();
    },
  });
  const close = () => {
    onClose();
    setName("");
    add.reset();
  };
  return (
    <Modal
      open={open}
      onOpenChange={(o) => {
        if (!o) close();
      }}
      title="Add read replica"
      description="Creates a new Postgres service that streams from this database."
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          add.mutate();
        }}
      >
        {add.error ? <Alert>{add.error.message}</Alert> : null}
        <Field label="Name" hint="Optional.">
          {(p) => <Input {...p} autoFocus placeholder={`${instance.serviceName}-replica`} value={name} onChange={(e) => setName(e.target.value)} />}
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={add.isPending}>
            Add replica
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function PromoteDialog({
  instance,
  open,
  onClose,
  onDone,
}: {
  instance: Instance;
  open: boolean;
  onClose: () => void;
  onDone: (r: Replication) => void;
}) {
  const [typed, setTyped] = useState("");
  const promote = useMutation({
    mutationFn: () => api.promoteReplica(instance.id, typed),
    onSuccess: (r) => {
      onDone(r);
      close();
    },
  });
  const close = () => {
    onClose();
    setTyped("");
    promote.reset();
  };
  return (
    <Modal
      open={open}
      onOpenChange={(o) => {
        if (!o) close();
      }}
      title="Promote replica"
      description={`${instance.serviceName} becomes a standalone primary that accepts writes and stops following its current primary. This can't be undone.`}
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (typed === instance.serviceName) promote.mutate();
        }}
      >
        {promote.error ? <Alert>{promote.error.message}</Alert> : null}
        <Field label={`Type ${instance.serviceName} to confirm`}>
          {(p) => <Input {...p} autoFocus autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />}
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={close}>
            Cancel
          </Button>
          <Button type="submit" variant="danger" loading={promote.isPending} disabled={typed !== instance.serviceName}>
            Promote
          </Button>
        </div>
      </form>
    </Modal>
  );
}
