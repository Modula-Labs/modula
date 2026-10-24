import { useQuery } from "@tanstack/react-query";
import { useSearch } from "@tanstack/react-router";
import { clsx } from "clsx";
import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Download, Search } from "@/components/ui/icons";
import { Select } from "@/components/ui/Input";
import { Alert, Skeleton } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import { api, type LogLine } from "@/lib/api";
import {
  LOG_RANGES,
  MAX_LOG_LINES,
  appendLines,
  formatLogFileLine,
  formatLogTime,
  highlightParts,
  logRangeMs,
  nextSince,
  type LogRangeId,
} from "@/lib/logs";
import { useProject } from "./ProjectContext";
import { HttpLogsView } from "./HttpLogsView";
import { LogsHistogram } from "./LogsHistogram";

const POLL_MS = 3000;

/** The project's Logs page: application logs, or the HTTP requests seen by the edge. */
export function LogsPage() {
  const [tab, setTab] = useState<"logs" | "http">("logs");
  return (
    <div className="flex h-full flex-col">
      <div className="px-6 pt-6">
        <Segmented
          label="Log type"
          value={tab}
          onChange={setTab}
          options={[
            { value: "logs", label: "Logs" },
            { value: "http", label: "HTTP" },
          ]}
        />
      </div>
      <div className="min-h-0 flex-1">{tab === "logs" ? <LogsView /> : <HttpLogsView />}</div>
    </div>
  );
}

/** Project logs for the selected environment: search, service and time filters, a volume histogram,
 *  and a live tail that polls for new lines. */
function LogsView() {
  const { project: p, env } = useProject();
  const envId = env?.id;
  const [input, setInput] = useState("");
  const [query, setQuery] = useState("");
  // A link (e.g. a cron run's logs) can preselect the service and a time window.
  const initial = useSearch({ strict: false }) as { service?: string; since?: number; until?: number };
  const [service, setService] = useState(initial.service ?? "");
  const [rangeId, setRangeId] = useState<LogRangeId>("1h");
  // A histogram bar narrows the window to its bucket; changing the range resets it.
  const [custom, setCustom] = useState<{ since: number; until: number } | undefined>(
    initial.since && initial.until ? { since: initial.since, until: initial.until } : undefined,
  );
  const [live, setLive] = useState(false);
  const [extra, setExtra] = useState<LogLine[]>([]);
  const box = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const cursor = useRef(0);

  useEffect(() => {
    const t = window.setTimeout(() => setQuery(input.trim()), 300);
    return () => window.clearTimeout(t);
  }, [input]);

  const logs = useQuery({
    queryKey: [
      "env-logs",
      envId,
      rangeId,
      custom?.since,
      custom?.until,
      query,
      service,
    ],
    queryFn: async () => {
      const until = custom?.until ?? Date.now();
      const since = custom?.since ?? until - logRangeMs(rangeId);
      const data = await api.environmentLogs(envId!, {
        since,
        until: custom?.until,
        q: query || undefined,
        service: service || undefined,
        limit: 1000,
      });
      return { data, since, until };
    },
    enabled: !!envId,
    placeholderData: (prev) => prev,
    refetchOnWindowFocus: false,
  });
  const base = logs.data;

  // A fresh result replaces whatever the live tail appended to the previous one.
  useEffect(() => {
    if (!base) return;
    setExtra([]);
    cursor.current = nextSince(base.data.lines, base.until);
    pinned.current = true;
  }, [base]);

  const lines = useMemo(
    () => (base ? appendLines(base.data.lines, extra, MAX_LOG_LINES) : []),
    [base, extra],
  );

  const liveOn = live && !custom;
  useEffect(() => {
    if (!liveOn || !envId || !base) return;
    let stopped = false;
    let busy = false;
    const tick = async () => {
      if (busy) return;
      busy = true;
      try {
        const r = await api.environmentLogs(envId, {
          since: cursor.current,
          q: query || undefined,
          service: service || undefined,
          limit: 1000,
        });
        if (stopped || r.lines.length === 0) return;
        cursor.current = nextSince(r.lines, cursor.current);
        setExtra((prev) => appendLines(prev, r.lines, MAX_LOG_LINES));
      } catch {
        // Transient; the next tick tries again.
      } finally {
        busy = false;
      }
    };
    const id = window.setInterval(() => void tick(), POLL_MS);
    return () => {
      stopped = true;
      window.clearInterval(id);
    };
  }, [liveOn, envId, base, query, service]);

  useEffect(() => {
    if (pinned.current && box.current)
      box.current.scrollTop = box.current.scrollHeight;
  }, [lines]);

  const download = () => {
    const href = URL.createObjectURL(
      new Blob([lines.map(formatLogFileLine).join("\n") + "\n"], {
        type: "text/plain",
      }),
    );
    const a = document.createElement("a");
    a.href = href;
    a.download = `${p?.name ?? "project"}-${env?.name ?? "env"}.log`;
    a.click();
    URL.revokeObjectURL(href);
  };

  const first = logs.isLoading || !env;
  return (
    <div className="flex h-full flex-col gap-4 p-6 pt-4">
      <div className="flex flex-wrap items-center gap-2">
        <label className="flex h-8 min-w-56 flex-1 items-center gap-2 rounded-lg border border-input px-2.5 text-muted-foreground focus-within:border-ring focus-within:ring-3 focus-within:ring-ring/50 dark:bg-input/30">
          <Search className="size-4" />
          <input
            className="min-w-0 flex-1 bg-transparent text-sm text-foreground outline-none placeholder:text-muted-foreground"
            placeholder="Filter and search logs"
            aria-label="Filter and search logs"
            value={input}
            onChange={(e) => setInput(e.target.value)}
          />
        </label>
        <div className="w-44 shrink-0">
          <Select
            aria-label="Service"
            value={service}
            onChange={(e) => setService(e.target.value)}
          >
            <option value="">All services</option>
            {p?.services.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}
              </option>
            ))}
          </Select>
        </div>
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
          <span
            className={clsx(
              "size-1.5 rounded-full",
              liveOn ? "bg-success" : "bg-muted-foreground",
            )}
          />
          Live
        </Button>
        <Button
          variant="ghost"
          icon={<Download />}
          disabled={lines.length === 0}
          onClick={download}
        >
          Download
        </Button>
      </div>

      {logs.error ? <Alert>{logs.error.message}</Alert> : null}
      {custom ? (
        <div className="flex items-center gap-2 text-sm text-muted-foreground">
          <span>
            Showing {formatLogTime(custom.since).slice(0, 8)} to{" "}
            {formatLogTime(custom.until).slice(0, 8)}
          </span>
          <Button
            variant="ghost"
            onClick={() => setCustom(undefined)}
          >
            Reset
          </Button>
        </div>
      ) : null}

      {first ? (
        <Skeleton className="h-[74px]" />
      ) : base ? (
        <LogsHistogram
          buckets={base.data.histogram}
          bucketMs={base.data.bucketMs}
          since={base.since}
          until={base.until}
          onSelect={setCustom}
        />
      ) : null}
      {base?.data.truncated ? (
        <p className="text-[13px] text-muted-foreground">
          Showing the newest {base.data.lines.length} lines. Narrow the time
          range or add a filter to see others.
        </p>
      ) : null}

      <div
        ref={box}
        onScroll={(e) => {
          const el = e.currentTarget;
          pinned.current =
            el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
        className="min-h-40 flex-1 overflow-auto rounded-xl border border-border bg-background py-2 font-mono text-[12.5px] leading-5"
        role="log"
        aria-live="off"
      >
        {first ? (
          <div className="px-3 text-muted-foreground">Loading logs…</div>
        ) : lines.length === 0 ? (
          <div className="flex h-full min-h-32 flex-col items-center justify-center gap-1 px-3 font-sans">
            <div className="text-sm font-bold text-foreground">
              No logs in this time range
            </div>
            <p className="text-sm text-muted-foreground">
              Try a longer range, another service, or clear the filter.
            </p>
          </div>
        ) : (
          lines.map((l, i) => (
            <div
              key={`${l.t}-${i}`}
              className={clsx(
                "flex gap-3 px-3 hover:bg-accent/60",
                l.stream === "stderr" &&
                  "border-l-2 border-destructive/70 bg-destructive/5 pl-[10px]",
              )}
            >
              <span
                className="shrink-0 text-muted-foreground tabular-nums select-none"
                title={new Date(l.t).toLocaleString()}
              >
                {formatLogTime(l.t)}
              </span>
              <span
                className="max-w-32 shrink-0 truncate rounded bg-muted px-1.5 text-[11px] leading-5 text-muted-foreground select-none"
                title={l.serviceName}
              >
                {l.serviceName}
              </span>
              <span className="min-w-0 break-all whitespace-pre-wrap">
                {highlightParts(l.line, query).map((part, j) =>
                  part.match ? (
                    <mark
                      key={j}
                      className="rounded-sm bg-warning/30 text-inherit"
                    >
                      {part.text}
                    </mark>
                  ) : (
                    part.text
                  ),
                )}
              </span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
