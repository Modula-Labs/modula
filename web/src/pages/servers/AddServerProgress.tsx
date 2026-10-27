import { useQuery } from "@tanstack/react-query";
import { clsx } from "clsx";
import { Button, Spinner } from "@/components/ui/Button";
import { Check } from "@/components/ui/icons";
import { Alert, Skeleton } from "@/components/ui/misc";
import { flagEmoji } from "@/lib/flags";
import {
  PROVISION_STEPS,
  euros,
  provisionApi,
  stepIndex,
  type ProvisionStatus,
} from "@/lib/provision";

const settled = (s: ProvisionStatus | undefined) =>
  s === "ready" || s === "failed";

/** Creating → Booting → Joining → Ready, polled live. */
export function AddServerProgress({
  serverId,
  onDone,
}: {
  serverId: string;
  onDone: () => void;
}) {
  const server = useQuery({
    queryKey: ["provisioned-server", serverId],
    queryFn: () => provisionApi.server(serverId),
    refetchInterval: (q) => (settled(q.state.data?.status) ? false : 2000),
  });
  if (server.isPending) return <Skeleton className="h-32" />;
  if (server.error) return <Alert>{server.error.message}</Alert>;
  const s = server.data;
  const current = stepIndex(s.status);

  return (
    <div className="flex flex-col gap-5">
      <div className="flex items-center gap-3 rounded-lg border border-border px-4 py-3 text-sm">
        <span aria-hidden>{flagEmoji(s.location?.countryCode)}</span>
        <span>{s.name}</span>
        <span className="font-mono text-muted-foreground">{s.serverType}</span>
        <span className="text-muted-foreground">{s.location?.city}</span>
        <span className="ml-auto tabular-nums text-muted-foreground">
          {euros(s.monthlyPriceCents)}/mo
        </span>
      </div>

      <ol className="flex flex-col gap-3" aria-label="Progress">
        {PROVISION_STEPS.map((step, i) => {
          const done = s.status === "ready" || (current >= 0 && i < current);
          const active = i === current && s.status !== "ready";
          return (
            <li
              key={step.status}
              className="flex items-center gap-3 text-sm"
              aria-current={active ? "step" : undefined}
            >
              <span
                className={clsx(
                  "flex size-6 items-center justify-center rounded-full border",
                  done
                    ? "border-success bg-badge-green-bg text-badge-green-fg"
                    : "border-border text-muted-foreground",
                )}
              >
                {done ? (
                  <Check className="size-3.5" />
                ) : active ? (
                  <Spinner className="size-3.5" />
                ) : (
                  <span className="size-1.5 rounded-full bg-current" />
                )}
              </span>
              <span
                className={clsx(
                  done || active ? "text-foreground" : "text-muted-foreground",
                )}
              >
                {step.label}
              </span>
              {active && step.status === "joining" ? (
                <span className="text-[13px] text-muted-foreground">
                  installing modula-node; usually under a minute
                </span>
              ) : null}
              {step.status === "booting" && s.ipv4 && (done || active) ? (
                <span className="font-mono text-[13px] text-muted-foreground">
                  {s.ipv4}
                </span>
              ) : null}
            </li>
          );
        })}
      </ol>

      {s.status === "failed" ? (
        <Alert title="The server did not come up">
          {s.error ?? "Unknown error."} Delete it from the servers list to free
          its place in the limits.
        </Alert>
      ) : null}

      <div className="flex justify-end">
        <Button variant="secondary" onClick={onDone}>
          {settled(s.status) ? "Done" : "Close — keep going in the background"}
        </Button>
      </div>
    </div>
  );
}
