import { useMutation } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { monthlyUsd, provisionApi, type ProviderStatus } from "@/lib/provision";

/**
 * The part of a connected cloud provider's settings card that is the same for
 * every provider: server and spend limits, what is in use, and disconnecting.
 */
export function ProviderLimits({
  workspaceId,
  kind,
  status,
  canEdit,
  usageNote,
  onChanged,
}: {
  workspaceId: string;
  /** Provider kind, e.g. `hetzner`. */
  kind: string;
  status: ProviderStatus;
  canEdit: boolean;
  /** What the spend covers, shown after the monthly total: "incl. VAT". */
  usageNote: string;
  onChanged: () => void;
}) {
  const [maxServers, setMaxServers] = useState("");
  const [maxDollars, setMaxDollars] = useState("");
  useEffect(() => {
    setMaxServers(String(status.caps.maxServers));
    setMaxDollars(String(status.caps.maxMonthlyCents / 100));
  }, [status.caps]);

  const saveCaps = useMutation({
    mutationFn: () =>
      provisionApi.updateCaps(workspaceId, kind, {
        maxServers: Number(maxServers),
        maxMonthlyCents: Math.round(Number(maxDollars) * 100),
      }),
    onSuccess: onChanged,
  });
  const disconnect = useMutation({
    mutationFn: () => provisionApi.disconnect(workspaceId, kind),
    onSuccess: onChanged,
  });

  const capsValid =
    /^\d+$/.test(maxServers) &&
    Number(maxDollars) >= 0 &&
    maxDollars.trim() !== "" &&
    !Number.isNaN(Number(maxDollars));
  const capsDirty =
    Number(maxServers) !== status.caps.maxServers ||
    Math.round(Number(maxDollars) * 100) !== status.caps.maxMonthlyCents;

  return (
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
        <Field label="Monthly limit ($)">
          {(p) => (
            <Input
              {...p}
              className="w-32"
              inputMode="decimal"
              disabled={!canEdit}
              value={maxDollars}
              onChange={(e) => setMaxDollars(e.target.value)}
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
        In use: {status.usage.servers} server{status.usage.servers === 1 ? "" : "s"}
        , {monthlyUsd(status.usage.monthlyCents, kind)} per month ({usageNote}).
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
  );
}
