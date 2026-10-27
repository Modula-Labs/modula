import { useQuery } from "@tanstack/react-query";
import { clsx } from "clsx";
import { Fragment, useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Search } from "@/components/ui/icons";
import { Select } from "@/components/ui/Input";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/Table";
import { api, type HttpRequest, type HttpStatusClass } from "@/lib/api";
import { formatBytes } from "@/lib/format";
import { HTTP_METHODS, formatDuration, newestFirst, statusTone } from "@/lib/http";
import { LOG_RANGES, formatLogTime, logRangeMs, type LogRangeId } from "@/lib/logs";
import { HttpHistogram } from "./HttpHistogram";
import { useProject } from "./ProjectContext";

const POLL_MS = 3000;
const STATUS_CLASSES: { value: "" | HttpStatusClass; label: string }[] = [
  { value: "", label: "All" },
  { value: "2xx", label: "2xx" },
  { value: "3xx", label: "3xx" },
  { value: "4xx", label: "4xx" },
  { value: "5xx", label: "5xx" },
];

/** HTTP requests of the selected environment as seen by the edge: filters, a volume histogram and a dense table.
 *  Live re-queries the window every few seconds. */
export function HttpLogsView() {
  const { project: p, env } = useProject();
  const envId = env?.id;
  const [input, setInput] = useState("");
  const [path, setPath] = useState("");
  const [service, setService] = useState("");
  const [statusFilter, setStatusFilter] = useState<"" | HttpStatusClass>("");
  const [method, setMethod] = useState("");
  const [rangeId, setRangeId] = useState<LogRangeId>("1h");
  // A histogram bar narrows the window to its bucket; changing the range resets it.
  const [custom, setCustom] = useState<{ since: number; until: number }>();
  const [live, setLive] = useState(false);
  const [open, setOpen] = useState<string | null>(null);

  useEffect(() => {
    const t = window.setTimeout(() => setPath(input.trim()), 300);
    return () => window.clearTimeout(t);
  }, [input]);

  const liveOn = live && !custom;
  const q = useQuery({
    queryKey: ["env-http-logs", envId, rangeId, custom?.since, custom?.until, path, service, statusFilter, method],
    queryFn: async () => {
      const until = custom?.until ?? Date.now();
      const since = custom?.since ?? until - logRangeMs(rangeId);
      const data = await api.environmentHttpLogs(envId!, {
        since,
        until: custom?.until,
        service: service || undefined,
        status: statusFilter || undefined,
        method: method || undefined,
        path: path || undefined,
        limit: 1000,
      });
      return { data, since, until };
    },
    enabled: !!envId,
    placeholderData: (prev) => prev,
    refetchOnWindowFocus: false,
    refetchInterval: liveOn ? POLL_MS : false,
  });
  const base = q.data;
  const rows = useMemo(() => (base ? newestFirst(base.data.requests) : []), [base]);
  const first = q.isLoading || !env;
  const filtered = !!(path || service || statusFilter || method);

  return (
    <div className="flex h-full flex-col gap-4 overflow-y-auto p-6 pt-4">
      <div className="flex flex-wrap items-center gap-2">
        <label className="flex h-8 min-w-48 flex-1 items-center gap-2 rounded-lg border border-input px-2.5 text-muted-foreground focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50 dark:bg-input/30">
          <Search className="size-4" />
          <input
            className="min-w-0 flex-1 bg-transparent text-sm text-foreground outline-none placeholder:text-muted-foreground"
            placeholder="Path contains"
            aria-label="Path contains"
            value={input}
            onChange={(e) => setInput(e.target.value)}
          />
        </label>
        <div className="w-40 shrink-0">
          <Select aria-label="Service" value={service} onChange={(e) => setService(e.target.value)}>
            <option value="">All services</option>
            {p?.services.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}
              </option>
            ))}
          </Select>
        </div>
        <div className="w-32 shrink-0">
          <Select aria-label="Method" value={method} onChange={(e) => setMethod(e.target.value)}>
            <option value="">All methods</option>
            {HTTP_METHODS.map((m) => (
              <option key={m} value={m}>
                {m}
              </option>
            ))}
          </Select>
        </div>
        <Segmented label="Status class" value={statusFilter} onChange={setStatusFilter} options={STATUS_CLASSES} />
        <div className="w-40 shrink-0">
          <Select
            aria-label="Time range"
            value={rangeId}
            onChange={(e) => {
              setRangeId(e.target.value as LogRangeId);
              setCustom(undefined);
            }}
          >
            {LOG_RANGES.map((r) => (
              <option key={r.id} value={r.id}>
                {r.label}
              </option>
            ))}
          </Select>
        </div>
        <Button
          variant="ghost"
          role="switch"
          aria-checked={liveOn}
          disabled={!!custom}
          title={custom ? "Reset the time range to follow live" : undefined}
          className={clsx(liveOn && "bg-accent text-foreground")}
          onClick={() => setLive((l) => !l)}
        >
          <span className={clsx("size-1.5 rounded-full", liveOn ? "bg-success" : "bg-muted-foreground")} />
          Live
        </Button>
      </div>

      {q.error ? <Alert>{q.error.message}</Alert> : null}
      {custom ? (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <span>
            Showing {formatLogTime(custom.since).slice(0, 8)} to {formatLogTime(custom.until).slice(0, 8)}
          </span>
          <Button variant="ghost" onClick={() => setCustom(undefined)}>
            Reset
          </Button>
        </div>
      ) : null}

      {first ? (
        <Skeleton className="h-[74px]" />
      ) : base ? (
        <HttpHistogram buckets={base.data.histogram} bucketMs={base.data.bucketMs} since={base.since} until={base.until} onSelect={setCustom} />
      ) : null}
      {base?.data.truncated ? (
        <p className="text-[13px] text-muted-foreground">Showing the newest {base.data.requests.length} requests. Narrow the time range or add a filter to see others.</p>
      ) : null}

      {first ? (
        <Skeleton className="h-48" />
      ) : rows.length === 0 ? (
        <div className="flex min-h-32 flex-col items-center justify-center gap-1 rounded-xl border border-dashed border-border px-3 py-10">
          <div className="text-sm font-bold">No requests in this time range</div>
          <p className="text-sm text-muted-foreground">
            {filtered ? "Try a longer range or clear the filters." : "Requests to your services' domains show up here a few seconds after they happen."}
          </p>
        </div>
      ) : (
        <Table className="text-[13px]">
          <TableHeader>
            <TableRow>
              <TableHead className="w-28">Time</TableHead>
              <TableHead className="w-20">Status</TableHead>
              <TableHead className="w-20">Method</TableHead>
              <TableHead>Host and path</TableHead>
              <TableHead className="w-24 text-right">Duration</TableHead>
              <TableHead className="w-24 text-right">Size</TableHead>
              <TableHead className="w-36">Client IP</TableHead>
              <TableHead className="w-32">Service</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((r, i) => {
              const key = `${r.t}-${i}`;
              const expanded = open === key;
              return (
                <Fragment key={key}>
                  <TableRow className="cursor-pointer" aria-selected={expanded} onClick={() => setOpen(expanded ? null : key)}>
                    <TableCell className="h-8 font-mono text-[12.5px] tabular-nums text-muted-foreground" title={new Date(r.t).toLocaleString()}>
                      {formatLogTime(r.t)}
                    </TableCell>
                    <TableCell className="h-8">
                      <StatusChip status={r.status} />
                    </TableCell>
                    <TableCell className="h-8 font-mono text-[12.5px]">{r.method}</TableCell>
                    <TableCell className="h-8 max-w-0 truncate font-mono text-[12.5px]" title={`${r.host}${r.path}`}>
                      <span className="text-muted-foreground">{r.host}</span>
                      {r.path}
                    </TableCell>
                    <TableCell className="h-8 text-right tabular-nums">{formatDuration(r.durationMs)}</TableCell>
                    <TableCell className="h-8 text-right tabular-nums">{formatBytes(r.bytes)}</TableCell>
                    <TableCell className="h-8 font-mono text-[12.5px]">{r.clientIp}</TableCell>
                    <TableCell className="h-8 max-w-0 truncate" title={r.serviceName}>
                      {r.serviceName}
                    </TableCell>
                  </TableRow>
                  {expanded ? <RequestDetail r={r} /> : null}
                </Fragment>
              );
            })}
          </TableBody>
        </Table>
      )}
    </div>
  );
}

/** Neutral for 2xx and 3xx, amber for 4xx, red for 5xx. */
function StatusChip({ status }: { status: number }) {
  return <Badge tone={statusTone(status)}>{status}</Badge>;
}

function RequestDetail({ r }: { r: HttpRequest }) {
  const rows: [string, string][] = [
    ["Time", new Date(r.t).toLocaleString()],
    ["URL", `${r.host}${r.path}`],
    ["User agent", r.userAgent || "—"],
    ["Upstream", r.upstream || "—"],
    ["Deployment", r.deploymentId],
  ];
  return (
    <TableRow className="hover:bg-transparent">
      <TableCell colSpan={8} className="h-auto bg-muted/30 py-2">
        <dl className="grid grid-cols-[100px_1fr] gap-x-3 gap-y-1 text-[13px]">
          {rows.map(([k, v]) => (
            <Fragment key={k}>
              <dt className="text-muted-foreground">{k}</dt>
              <dd className="min-w-0 break-all font-mono text-[12.5px]">{v}</dd>
            </Fragment>
          ))}
        </dl>
      </TableCell>
    </TableRow>
  );
}
