import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Disk, Plug, Trash } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { api, type Instance } from "@/lib/api";
import { defaultMountPath } from "@/lib/templates";

function useRefresh(instance: Instance) {
  const qc = useQueryClient();
  return (i?: Instance) => {
    if (i) qc.setQueryData(["instance", instance.id], i);
    void qc.invalidateQueries({ queryKey: ["instance", instance.id] });
    void qc.invalidateQueries({ queryKey: ["project", instance.projectId] });
    void qc.invalidateQueries({ queryKey: ["pending", instance.environmentId] });
  };
}

/** Attach, show and delete the instance's single volume. */
export function VolumeSection({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const refresh = useRefresh(instance);
  const [path, setPath] = useState(() => defaultMountPath(instance.template));
  const [confirm, setConfirm] = useState(false);
  const [typed, setTyped] = useState("");
  const volume = instance.volume;
  const attach = useMutation({ mutationFn: () => api.createVolume(instance.id, path.trim()), onSuccess: refresh });
  const remove = useMutation({
    mutationFn: () => api.deleteVolume(volume!.id),
    onSuccess: () => {
      setConfirm(false);
      setTyped("");
      refresh();
    },
  });
  const pathError = path.trim() && !path.trim().startsWith("/") ? "Use an absolute path" : null;

  return (
    <>
      {attach.error ? <Alert>{attach.error.message}</Alert> : null}
      {volume ? (
        <div className="flex h-10 items-center gap-2 rounded-md border border-border pr-1 pl-3">
          <Disk className="size-4 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate font-mono text-[13px]">{volume.mountPath}</span>
          <CopyButton value={volume.mountPath} label="Copy mount path" />
          {canEdit ? (
            <button
              aria-label="Delete volume"
              onClick={() => setConfirm(true)}
              className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-error"
            >
              <Trash className="size-4" />
            </button>
          ) : null}
        </div>
      ) : canEdit ? (
        <form
          className="flex items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (!pathError && path.trim()) attach.mutate();
          }}
        >
          <div className="flex-1">
            <Field label="Mount path" hint="Where the volume appears inside the container." error={pathError}>
              {(p) => <Input {...p} mono value={path} onChange={(e) => setPath(e.target.value)} />}
            </Field>
          </div>
          <Button type="submit" variant="secondary" icon={<Disk />} loading={attach.isPending} disabled={!path.trim() || !!pathError} className="mb-[22px]">
            Attach volume
          </Button>
        </form>
      ) : (
        <p className="text-sm text-muted-foreground">No volume attached.</p>
      )}
      <p className="text-xs text-muted-foreground">
        A service with a volume redeploys with brief downtime: the old container stops before the new one starts, so two never write to the data at once.
      </p>

      <Modal
        open={confirm}
        onOpenChange={(o) => {
          setConfirm(o);
          if (!o) {
            setTyped("");
            remove.reset();
          }
        }}
        title="Delete volume"
        description={`This permanently deletes all data stored at ${volume?.mountPath ?? ""}. It can't be undone.`}
      >
        <form
          className="flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (typed === instance.serviceName) remove.mutate();
          }}
        >
          {remove.error ? <Alert>{remove.error.message}</Alert> : null}
          <Field label={`Type ${instance.serviceName} to confirm`}>
            {(p) => <Input {...p} autoFocus autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />}
          </Field>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => setConfirm(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="danger" loading={remove.isPending} disabled={typed !== instance.serviceName}>
              Delete volume
            </Button>
          </div>
        </form>
      </Modal>
    </>
  );
}

/** Public TCP access to a port, for clients that don't speak HTTP (database tools). */
export function TcpProxySection({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const refresh = useRefresh(instance);
  const [port, setPort] = useState(String(instance.config.port));
  const proxy = instance.tcpProxy;
  const enable = useMutation({ mutationFn: () => api.createTcpProxy(instance.id, Number(port)), onSuccess: refresh });
  const remove = useMutation({ mutationFn: () => api.deleteTcpProxy(proxy!.id), onSuccess: () => refresh() });
  const error = enable.error ?? remove.error;
  const address = proxy ? `${proxy.host}:${proxy.publicPort}` : "";

  return (
    <div className="flex flex-col gap-2">
      <div className="text-sm font-medium">TCP proxy</div>
      {error ? <Alert>{error.message}</Alert> : null}
      {proxy ? (
        <div className="flex h-10 items-center gap-2 rounded-md border border-border pr-1 pl-3">
          <Plug className="size-4 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate font-mono text-[13px]">{address}</span>
          <span className="text-xs text-muted-foreground">→ {proxy.targetPort}</span>
          <CopyButton value={address} label="Copy address" />
          {canEdit ? (
            <button
              aria-label="Remove TCP proxy"
              onClick={() => remove.mutate()}
              className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-error"
            >
              <Trash className="size-4" />
            </button>
          ) : null}
        </div>
      ) : canEdit ? (
        <form
          className="flex items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (Number(port)) enable.mutate();
          }}
        >
          <Field label="Target port" hint="The port inside the container to expose publicly.">
            {(p) => <Input {...p} className="w-32" inputMode="numeric" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />}
          </Field>
          <Button type="submit" variant="secondary" icon={<Plug />} loading={enable.isPending} disabled={!Number(port)} className="mb-[22px]">
            Enable TCP proxy
          </Button>
        </form>
      ) : (
        <p className="text-sm text-muted-foreground">Not enabled.</p>
      )}
    </div>
  );
}
