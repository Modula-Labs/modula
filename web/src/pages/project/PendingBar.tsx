import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Check, ChevronDown, ChevronRight } from "@/components/ui/icons";
import { Alert } from "@/components/ui/misc";
import { ApiError, api, type PendingChanges } from "@/lib/api";
import { describeChange } from "./activity";

const none: PendingChanges = { count: 0, services: [] };

/** Edits not yet deployed in this environment. Shared with the page so it can
 *  demote its own primary button while the bar is up. */
export function usePending(environmentId: string | undefined) {
  return useQuery({
    queryKey: ["pending", environmentId],
    enabled: !!environmentId,
    queryFn: async () => {
      try {
        return await api.pending(environmentId!);
      } catch (e) {
        // Older servers don't track pending changes.
        if (e instanceof ApiError && e.status === 404) return none;
        throw e;
      }
    },
    refetchInterval: 10_000,
    retry: false,
  });
}

/** Floating "Deploy N changes" bar at the bottom of the canvas. */
export function PendingBar({ projectId, environmentId }: { projectId: string; environmentId: string }) {
  const qc = useQueryClient();
  const pending = usePending(environmentId).data ?? none;
  const [open, setOpen] = useState(false);
  const [done, setDone] = useState<number | null>(null);
  useEffect(() => {
    if (done === null) return;
    const t = window.setTimeout(() => setDone(null), 4000);
    return () => window.clearTimeout(t);
  }, [done]);

  const deploy = useMutation({
    mutationFn: (serviceIds?: string[]) => api.deployPending(environmentId, serviceIds),
    onSuccess: async (r) => {
      setDone(r.deployments.length);
      await Promise.all([
        qc.invalidateQueries({ queryKey: ["pending", environmentId] }),
        qc.invalidateQueries({ queryKey: ["project", projectId] }),
        qc.invalidateQueries({ queryKey: ["deployments"] }),
        qc.invalidateQueries({ queryKey: ["activity"] }),
      ]);
    },
  });

  if (pending.count === 0 && done === null && !deploy.error) return null;
  const n = pending.count;
  return (
    <div className="anim-sheet absolute bottom-4 left-1/2 z-10 w-[480px] max-w-[calc(100%-24px)] -translate-x-1/2 rounded-xl border border-border bg-card shadow-modal">
      {deploy.error ? (
        <div className="p-2 pb-0">
          <Alert>{deploy.error.message}</Alert>
        </div>
      ) : null}
      {open && n > 0 ? (
        <div className="max-h-72 overflow-y-auto border-b border-border p-3">
          <div className="flex flex-col gap-3">
            {pending.services.map((s) => (
              <section key={s.serviceId} aria-label={s.serviceName}>
                <div className="flex items-center justify-between gap-2">
                  <h3 className="truncate text-sm font-semibold">{s.serviceName}</h3>
                  <Button size="sm" variant="ghost" disabled={deploy.isPending} onClick={() => deploy.mutate([s.serviceId])}>
                    Deploy
                  </Button>
                </div>
                <ul className="mt-1 flex flex-col gap-0.5 text-sm text-muted-foreground">
                  {s.changes.map((c, i) => (
                    <li key={i} className="truncate">
                      {describeChange(c)}
                    </li>
                  ))}
                </ul>
              </section>
            ))}
          </div>
        </div>
      ) : null}
      <div className="flex items-center gap-2 p-2 pl-3">
        {n > 0 ? (
          <>
            <button
              aria-expanded={open}
              onClick={() => setOpen((o) => !o)}
              className="flex h-8 items-center gap-1.5 rounded-md pr-2 text-sm font-medium hover:text-foreground"
            >
              {open ? <ChevronDown className="size-4 text-muted-foreground" /> : <ChevronRight className="size-4 text-muted-foreground" />}
              {n} {n === 1 ? "change" : "changes"}
            </button>
            <span className="min-w-0 flex-1 truncate text-sm text-muted-foreground">
              in {pending.services.length} {pending.services.length === 1 ? "service" : "services"}
            </span>
            <Button variant="primary" size="sm" loading={deploy.isPending} onClick={() => deploy.mutate(undefined)}>
              Deploy
            </Button>
          </>
        ) : (
          <span className="flex h-8 items-center gap-2 text-sm">
            <Check className="size-4 text-success" />
            {done === null ? "Nothing to deploy" : `Deploying ${done} ${done === 1 ? "service" : "services"}`}
          </span>
        )}
      </div>
    </div>
  );
}
