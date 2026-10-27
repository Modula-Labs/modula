import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { api, type Environment, type SyncResult } from "@/lib/api";

/** Copies services and configuration from another environment into this one. */
export function SyncDialog({
  open,
  onOpenChange,
  projectId,
  target,
  environments,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  projectId: string;
  target: Environment;
  environments: Environment[];
}) {
  const qc = useQueryClient();
  const sources = environments.filter((e) => e.id !== target.id);
  const [from, setFrom] = useState("");
  const [result, setResult] = useState<SyncResult | null>(null);
  useEffect(() => {
    if (open) {
      setFrom(sources[0]?.id ?? "");
      setResult(null);
    }
    // Reset only when the dialog opens or the target changes.
  }, [open, target.id]);
  const sync = useMutation({
    mutationFn: () => api.syncEnvironment(target.id, from),
    onSuccess: async (r) => {
      setResult(r);
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
    },
  });
  const fromName = sources.find((e) => e.id === from)?.name;
  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      title="Sync environment"
      description={`Bring services and settings from another environment into ${target.name}.`}
    >
      {result ? (
        <div className="flex flex-col gap-4">
          <Alert tone="success">
            Synced from {fromName}: {result.created} created, {result.updated} updated.
          </Alert>
          <div className="flex justify-end">
            <Button variant="secondary" onClick={() => onOpenChange(false)}>
              Done
            </Button>
          </div>
        </div>
      ) : (
        <form
          className="flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            sync.mutate();
          }}
        >
          {sync.error ? <Alert>{sync.error.message}</Alert> : null}
          <Field label="Source environment">
            {(p) => (
              <Select {...p} value={from} onChange={(e) => setFrom(e.target.value)}>
                {sources.map((e) => (
                  <option key={e.id} value={e.id}>
                    {e.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="primary" loading={sync.isPending} disabled={!from}>
              Sync
            </Button>
          </div>
        </form>
      )}
    </Modal>
  );
}
