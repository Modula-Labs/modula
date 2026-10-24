import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Button } from "@/components/ui/Button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/Table";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { api, type CronRun, type Instance } from "@/lib/api";
import { cronLogsWindow, cronRunBadge, cronRunDuration, isCronRunActive } from "@/lib/cron";
import { formatUtc, relativeTime, relativeUntil } from "@/lib/format";

/** A cron service's schedule, the next run, a Run now button, and the history of its runs. */
export function CronTab({ instance, canRun }: { instance: Instance; canRun: boolean }) {
  const qc = useQueryClient();
  const runs = useQuery({
    queryKey: ["cron-runs", instance.id],
    queryFn: () => api.cronRuns(instance.id),
    refetchInterval: (q) => (q.state.data?.runs.some(isCronRunActive) ? 5000 : 30_000),
  });
  const runNow = useMutation({
    mutationFn: () => api.runCronNow(instance.id),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["cron-runs", instance.id] });
      void qc.invalidateQueries({ queryKey: ["deployments", instance.id] });
    },
  });

  if (runs.isPending) return <Skeleton className="h-40" />;
  const { schedule, runs: list = [] } = runs.data ?? { schedule: null };
  const error = runNow.error ?? runs.error;
  const next = schedule?.deployed ? schedule.upcoming[0] : undefined;
  return (
    <div className="flex flex-col gap-6">
      {error ? <Alert>{error.message}</Alert> : null}
      <div className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-border bg-card p-4">
        <div className="min-w-0">
          {schedule ? (
            <>
              <p className="text-sm font-bold">
                <span className="font-mono">{schedule.expression}</span> · {schedule.description}
              </p>
              <p className="text-sm text-muted-foreground">
                {next
                  ? `Next run ${relativeUntil(next)} (${formatUtc(next)})`
                  : schedule.deployed
                    ? "No upcoming runs"
                    : "Deploy to start this schedule"}
              </p>
            </>
          ) : (
            <p className="text-sm text-muted-foreground">This service has no cron schedule.</p>
          )}
        </div>
        {canRun ? (
          <Button variant="secondary" loading={runNow.isPending} disabled={!schedule?.deployed} onClick={() => runNow.mutate()}>
            Run now
          </Button>
        ) : null}
      </div>
      {list.length ? (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Status</TableHead>
              <TableHead>Trigger</TableHead>
              <TableHead>Started</TableHead>
              <TableHead>Duration</TableHead>
              <TableHead>Result</TableHead>
              <TableHead>
                <span className="sr-only">Logs</span>
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {list.map((r) => (
              <RunRow key={r.id} run={r} instance={instance} />
            ))}
          </TableBody>
        </Table>
      ) : (
        <p className="rounded-xl border border-dashed border-border px-4 py-6 text-center text-sm text-muted-foreground">
          No runs yet.
        </p>
      )}
    </div>
  );
}

function RunRow({ run: r, instance }: { run: CronRun; instance: Instance }) {
  const badge = cronRunBadge[r.status];
  const span = cronLogsWindow(r);
  const at = r.startedAt ?? r.scheduledFor ?? r.createdAt;
  return (
    <TableRow>
      <TableCell>
        <Badge tone={badge.tone}>{badge.label}</Badge>
      </TableCell>
      <TableCell className="capitalize">{r.trigger}</TableCell>
      <TableCell className="whitespace-nowrap" title={formatUtc(at)}>
        {r.startedAt ? relativeTime(r.startedAt) : r.scheduledFor ? `due ${relativeTime(r.scheduledFor)}` : "—"}
      </TableCell>
      <TableCell className="whitespace-nowrap">{cronRunDuration(r)}</TableCell>
      <TableCell className="max-w-64 truncate" title={r.error ?? undefined}>
        {r.error ? <span className="text-error">{r.error}</span> : r.exitCode !== null ? `Exit ${r.exitCode}` : "—"}
      </TableCell>
      <TableCell className="text-right">
        {span ? (
          <Link
            to="/projects/$projectId/logs"
            params={{ projectId: instance.projectId }}
            search={{ env: instance.environmentId, service: instance.serviceId, since: span.since, until: span.until } as never}
            className="text-sm underline-offset-4 hover:underline"
          >
            Logs
          </Link>
        ) : null}
      </TableCell>
    </TableRow>
  );
}
