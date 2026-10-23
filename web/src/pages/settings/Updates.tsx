import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input, Select } from "@/components/ui/Input";
import { Alert, Badge, Skeleton, type Tone } from "@/components/ui/misc";
import { api, ApiError, type UpdateMode, type UpdateRun, type UpdateServer, type UpdateStatus, type UpdateWindow } from "@/lib/api";
import { relativeTime } from "@/lib/format";
import { canUpdateNow, formatWindow, isRunActive, logTail } from "@/lib/updates";

const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MODES: { value: UpdateMode; label: string; hint: string }[] = [
  { value: "off", label: "Off", hint: "Never check for or show updates." },
  { value: "notify", label: "Notify only", hint: "Show an update badge. You install it yourself." },
  { value: "auto", label: "Automatic", hint: "Installs new versions during the maintenance window." },
];
const RUN_TONE: Record<UpdateRun["status"], Tone> = { requested: "info", running: "info", succeeded: "success", failed: "danger", rolled_back: "warn" };
const SERVER_TONE: Record<UpdateServer["status"], Tone> = { current: "success", behind: "neutral", upgrading: "info", failed: "danger", manual: "warn", offline: "neutral" };
const SERVER_LABEL: Record<UpdateServer["status"], string> = { current: "Current", behind: "Waiting", upgrading: "Upgrading", failed: "Failed", manual: "Manual", offline: "Offline" };

function timezones(extra: string): string[] {
  let list: string[] = [];
  try {
    list = (Intl as unknown as { supportedValuesOf?: (k: string) => string[] }).supportedValuesOf?.("timeZone") ?? [];
  } catch {
    list = [];
  }
  const set = new Set(list);
  set.add("UTC");
  if (extra) set.add(extra);
  return [...set].sort();
}

function sameWindow(a: UpdateWindow, b: UpdateWindow) {
  return a.time === b.time && a.timezone === b.timezone && [...a.days].sort().join() === [...b.days].sort().join();
}

export function UpdatesSection() {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["instance-updates"],
    queryFn: api.updates,
    retry: (n, e) => !(e instanceof ApiError && e.status === 404) && n < 2,
    refetchInterval: (query) => (isRunActive(query.state.data) ? 5000 : 60000),
  });
  const [mode, setMode] = useState<UpdateMode>("notify");
  const [win, setWin] = useState<UpdateWindow>({ days: [], time: "03:00", timezone: "UTC" });
  useEffect(() => {
    if (q.data) {
      setMode(q.data.mode);
      setWin(q.data.window);
    }
  }, [q.data?.mode, q.data?.window.time, q.data?.window.timezone, q.data?.window.days.join()]); // eslint-disable-line react-hooks/exhaustive-deps

  const set = (s: UpdateStatus) => qc.setQueryData(["instance-updates"], s);
  const save = useMutation({ mutationFn: () => api.saveUpdates({ mode, channel: "stable", window: win }), onSuccess: set });
  const run = useMutation({ mutationFn: api.runUpdate, onSuccess: set });
  const check = useMutation({ mutationFn: api.checkUpdates, onSuccess: set });

  if (q.error instanceof ApiError && q.error.status === 404) return null;
  if (q.isPending) return <Skeleton className="h-40" />;
  if (q.error) return <Alert>{q.error.message}</Alert>;

  const s = q.data;
  const dirty = mode !== s.mode || !sameWindow(win, s.window);
  const error = save.error ?? run.error ?? check.error;
  const toggleDay = (d: number) => setWin((w) => ({ ...w, days: w.days.includes(d) ? w.days.filter((x) => x !== d) : [...w.days, d] }));
  const active = isRunActive(s);

  return (
    <div className="flex flex-col gap-5">
      {error ? <Alert>{error.message}</Alert> : null}

      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="text-sm">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-mono">{s.currentVersion}</span>
            {s.updateAvailable && s.latestVersion ? <Badge tone="info">Update available</Badge> : <Badge tone="success">Up to date</Badge>}
          </div>
          {s.updateAvailable && s.latestVersion ? (
            <div className="mt-1.5">
              Update available: <span className="font-mono">v{s.currentVersion}</span> → <span className="font-mono">v{s.latestVersion}</span>
              {s.releaseNotesUrl ? (
                <>
                  {" "}
                  <a href={s.releaseNotesUrl} target="_blank" rel="noreferrer" className="underline underline-offset-2">Release notes</a>
                </>
              ) : null}
            </div>
          ) : null}
          <div className="mt-1 text-muted-foreground">
            Latest <span className="font-mono">{s.latestVersion ?? "unknown"}</span>
            {s.checkedAt ? ` · checked ${relativeTime(s.checkedAt)}` : " · not checked yet"}
          </div>
          {s.checkError ? <div className="mt-1 text-warning">Last check failed: {s.checkError}</div> : null}
        </div>
        <Button variant="ghost" loading={check.isPending} onClick={() => check.mutate()}>Check now</Button>
      </div>

      <div className="flex flex-col gap-3">
        <Field label="Updates" hint={MODES.find((m) => m.value === mode)?.hint}>
          {(p) => (
            <Select {...p} value={mode} onChange={(e) => setMode(e.target.value as UpdateMode)}>
              {MODES.map((m) => (
                <option key={m.value} value={m.value}>{m.label}</option>
              ))}
            </Select>
          )}
        </Field>
        <div className="flex flex-col gap-1.5">
          <span className="text-sm font-medium">Maintenance window</span>
          <div className="flex flex-wrap gap-1.5" role="group" aria-label="Days">
            {DAYS.map((d, i) => (
              <Button key={d} size="sm" variant={win.days.includes(i) ? "secondary" : "outline"} aria-pressed={win.days.includes(i)} onClick={() => toggleDay(i)}>
                {d}
              </Button>
            ))}
          </div>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Time">{(p) => <Input {...p} type="time" value={win.time} onChange={(e) => setWin((w) => ({ ...w, time: e.target.value }))} />}</Field>
            <Field label="Timezone">
              {(p) => (
                <Select {...p} value={win.timezone} onChange={(e) => setWin((w) => ({ ...w, timezone: e.target.value }))}>
                  {timezones(win.timezone).map((z) => (
                    <option key={z} value={z}>{z}</option>
                  ))}
                </Select>
              )}
            </Field>
          </div>
          <p className="text-xs text-muted-foreground">
            {formatWindow(win)}.
            {mode === "auto" && s.nextWindow ? ` Next window: ${new Date(s.nextWindow).toLocaleString()}.` : ""}
          </p>
        </div>
        <div>
          <Button variant="secondary" loading={save.isPending} disabled={!dirty || !win.time} onClick={() => save.mutate()}>Save</Button>
        </div>
      </div>

      <div className="flex flex-col gap-1.5 border-t border-border pt-4">
        <div>
          <Button variant="primary" loading={run.isPending || active} disabled={!canUpdateNow(s)} onClick={() => run.mutate()}>Update now</Button>
        </div>
        {!s.canUpgrade ? (
          <p className="text-xs text-muted-foreground">This instance was not installed with install.sh, so it cannot update itself.</p>
        ) : !s.updateAvailable ? (
          <p className="text-xs text-muted-foreground">Nothing to install.</p>
        ) : null}
      </div>

      {s.lastRun ? <LastRun run={s.lastRun} /> : null}

      {s.servers.length > 0 ? (
        <div className="flex flex-col gap-2">
          <div className="text-sm font-medium">Servers</div>
          <ul className="divide-y divide-border rounded-lg border border-border">
            {s.servers.map((sv) => (
              <li key={sv.nodeId} className="flex items-center justify-between gap-3 px-3 py-2 text-sm">
                <div className="min-w-0">
                  <div className="truncate">{sv.name}</div>
                  {sv.error ? <div className="truncate text-xs text-error">{sv.error}</div> : null}
                  {sv.status === "manual" ? <div className="text-xs text-muted-foreground">Too old to self-upgrade. Run install.sh on this server.</div> : null}
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  <span className="font-mono text-xs text-muted-foreground">{sv.version ?? "unknown"}</span>
                  <Badge tone={SERVER_TONE[sv.status]}>{SERVER_LABEL[sv.status]}</Badge>
                </div>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

function LastRun({ run }: { run: UpdateRun }) {
  const tail = logTail(run.log);
  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border bg-card p-3 text-sm">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">Last update</span>
        <Badge tone={RUN_TONE[run.status]}>{run.status.replace("_", " ")}</Badge>
        <span className="font-mono text-xs">{run.fromVersion} → {run.toVersion}</span>
      </div>
      <div className="text-xs text-muted-foreground">
        {run.trigger === "auto" ? "Automatic" : "Manual"} · requested {relativeTime(run.requestedAt)}
        {run.finishedAt ? ` · finished ${relativeTime(run.finishedAt)}` : ""}
      </div>
      {tail ? (
        <details>
          <summary className="cursor-pointer text-xs text-muted-foreground">Log</summary>
          <pre className="mt-2 max-h-64 overflow-auto rounded-md bg-muted p-2 font-mono text-xs whitespace-pre-wrap">{tail}</pre>
        </details>
      ) : null}
    </div>
  );
}
