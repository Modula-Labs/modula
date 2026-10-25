import { clsx } from "clsx";
import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Segmented } from "@/components/ui/Tabs";
import { api, type HttpRequest } from "@/lib/api";
import { formatBytes } from "@/lib/format";
import { formatMs, parseHttpSearch, statusTextClass } from "@/lib/http";
import { RETENTION_MS, formatLogDateTime, timezoneLabel, type LogLevel } from "@/lib/logs";
import { LogDetailPane, type LogDetail } from "./LogDetailPane";
import { JumpButtons, LogSearch, atBottom, revealRow } from "./logParts";

const POLL_MS = 3000;
const PAGE = 200;
const MAX_ROWS = 5000;

/** A request with a stable key (requests have no id). */
interface Row extends HttpRequest {
  key: number;
}

/** One deployment's HTTP requests as the edge saw them, Railway-style: a token
 *  search, a dense table with the newest at the bottom (followed live while
 *  scrolled to the bottom) and the shared detail pane. */
export function NetworkLogs({
  environmentId,
  deploymentId,
}: {
  environmentId: string;
  deploymentId: string;
}) {
  const [input, setInput] = useState("");
  const [query, setQuery] = useState("");
  const [rows, setRows] = useState<Row[]>([]);
  const [loading, setLoading] = useState(true);
  const [more, setMore] = useState<{ loading: boolean; done: boolean }>({ loading: false, done: false });
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [expanded, setExpanded] = useState(true);
  const [reveal, setReveal] = useState<number | null>(null);
  const box = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const pinned = useRef(true);
  const seq = useRef({ next: 0, prev: -1 });
  const rowsRef = useRef<Row[]>([]);
  rowsRef.current = rows;

  useEffect(() => {
    const t = window.setTimeout(() => setQuery(input), 300);
    return () => window.clearTimeout(t);
  }, [input]);
  const parsed = useMemo(() => parseHttpSearch(query), [query]);
  const filters = useMemo(
    () => ({ path: parsed.path, method: parsed.method, status: parsed.status, deployment: deploymentId }),
    [parsed.path, parsed.method, parsed.status, deploymentId],
  );

  // The newest page, then polling for anything newer while scrolled to the bottom.
  useEffect(() => {
    let stop = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    seq.current = { next: 0, prev: -1 };
    setRows([]);
    setSelected(null);
    setLoading(true);
    setError(null);
    setMore({ loading: false, done: false });
    pinned.current = true;
    const keyed = (rs: HttpRequest[]) => rs.map((r) => ({ ...r, key: seq.current.next++ }));

    const poll = async () => {
      if (stop) return;
      const last = rowsRef.current[rowsRef.current.length - 1];
      if (pinned.current && last) {
        try {
          const found = await api.environmentHttpLogs(environmentId, { ...filters, since: last.t + 1, limit: 1000 });
          if (stop) return;
          if (found.requests.length) {
            setRows((prev) => {
              const all = prev.concat(keyed(found.requests));
              return all.length > MAX_ROWS ? all.slice(all.length - MAX_ROWS) : all;
            });
          }
          setError(null);
        } catch (e) {
          if (!stop) setError((e as Error).message);
        }
      } else if (pinned.current && !last) {
        await first();
        return;
      }
      if (!stop) timer = setTimeout(poll, POLL_MS);
    };
    const first = async () => {
      try {
        const now = Date.now();
        const found = await api.environmentHttpLogs(environmentId, { ...filters, since: now - RETENTION_MS, limit: PAGE });
        if (stop) return;
        setRows(keyed(found.requests));
        setMore({ loading: false, done: !found.truncated });
        setError(null);
      } catch (e) {
        if (!stop) setError((e as Error).message);
      } finally {
        if (!stop) {
          setLoading(false);
          timer = setTimeout(poll, POLL_MS);
        }
      }
    };
    void first();
    return () => {
      stop = true;
      clearTimeout(timer);
    };
  }, [environmentId, filters]);

  useEffect(() => {
    if (pinned.current && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [rows]);

  useEffect(() => {
    if (reveal === null) return;
    revealRow(box.current, String(reveal));
    setReveal(null);
  }, [reveal]);

  const oldest = rows[0];
  const loadOlder = async () => {
    if (!oldest) return;
    setMore((m) => ({ ...m, loading: true }));
    try {
      const found = await api.environmentHttpLogs(environmentId, {
        ...filters,
        since: oldest.t - RETENTION_MS,
        until: oldest.t,
        limit: PAGE,
      });
      const el = box.current;
      const fromBottom = el ? el.scrollHeight - el.scrollTop : 0;
      const got: Row[] = [];
      for (let i = found.requests.length - 1; i >= 0; i--) got.unshift({ ...found.requests[i]!, key: seq.current.prev-- });
      setRows((prev) => [...got, ...prev]);
      setMore({ loading: false, done: !found.truncated });
      requestAnimationFrame(() => {
        if (el) el.scrollTop = el.scrollHeight - fromBottom;
      });
    } catch (e) {
      setError((e as Error).message);
      setMore((m) => ({ ...m, loading: false }));
    }
  };
  const canLoadOlder = !!oldest && !more.done && oldest.t > Date.now() - RETENTION_MS + 60_000;

  const at = selected === null ? -1 : rows.findIndex((r) => r.key === selected);
  const current = at >= 0 ? rows[at]! : null;
  const detail = useMemo(() => (current ? requestDetail(current) : null), [current]);
  const invalid = parsed.invalid.length > 0;

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <div className="flex items-center gap-2">
        <Segmented label="Network log type" value="http" onChange={() => {}} options={[{ value: "http", label: "HTTP" }]} />
        <LogSearch
          ref={search}
          value={input}
          onChange={setInput}
          placeholder="Search HTTP logs e.g. @path:/api/users @httpStatus:200"
          slashFocus
          invalid={invalid}
        />
      </div>
      {invalid ? (
        <p className="text-sm text-error">
          Ignored {parsed.invalid.join(", ")}. Use @path:, @method: or @httpStatus: (a code like 404, or 2xx to 5xx).
        </p>
      ) : null}
      {error ? <p className="text-sm text-error">{error}</p> : null}

      <div className="relative flex min-h-64 flex-1 flex-col">
        <div className="relative flex min-h-0 flex-1 flex-col overflow-hidden rounded-xl border border-border bg-background">
          <div className="flex h-8 shrink-0 items-center gap-3 border-b border-border px-3 font-mono text-[12.5px] text-muted-foreground">
            <span className="w-[19ch] shrink-0">Time ({timezoneLabel()})</span>
            <span className="w-[7ch] shrink-0">Method</span>
            <span className="min-w-0 flex-1">Path</span>
            <span className="w-[11ch] shrink-0">HTTP Status</span>
            <span className="w-[14ch] shrink-0 text-right">Total Duration</span>
          </div>
          <div
            ref={box}
            onScroll={(e) => {
              pinned.current = atBottom(e.currentTarget);
            }}
            className="min-h-0 flex-1 overflow-auto py-1 font-mono text-[12.5px] leading-6"
          >
            {canLoadOlder ? (
              <div className="px-3 py-1.5">
                <Button variant="outline" size="xs" loading={more.loading} onClick={loadOlder}>
                  Load more
                </Button>
              </div>
            ) : null}
            {rows.length === 0 ? (
              <div className="px-3 font-sans text-sm text-muted-foreground">
                {loading ? "Loading requests…" : query.trim() ? "No requests match." : "No requests yet."}
              </div>
            ) : (
              rows.map((r) => (
                <div
                  key={r.key}
                  data-row={r.key}
                  onClick={() => {
                    if (window.getSelection()?.toString()) return;
                    setSelected(r.key);
                  }}
                  className={clsx(
                    "flex cursor-pointer items-center gap-3 px-3 transition-colors hover:bg-accent/60",
                    r.key === selected && "bg-accent",
                  )}
                >
                  <span className="w-[19ch] shrink-0 text-muted-foreground tabular-nums">{formatLogDateTime(r.t)}</span>
                  <span className="w-[7ch] shrink-0">{r.method}</span>
                  <span className="min-w-0 flex-1 truncate" title={r.path}>
                    {r.path}
                  </span>
                  <span className={clsx("w-[11ch] shrink-0 tabular-nums", statusTextClass(r.status))}>{r.status}</span>
                  <span className="w-[14ch] shrink-0 text-right tabular-nums">{formatMs(r.durationMs)}</span>
                </div>
              ))
            )}
          </div>
          <JumpButtons box={box} />
        </div>

        {detail ? (
          <div className={clsx("flex flex-col", expanded ? "absolute inset-0 z-10" : "mt-2 h-[45%] min-h-48")}>
            <LogDetailPane
              detail={detail}
              expanded={expanded}
              onToggleExpanded={() => setExpanded((x) => !x)}
              onPrev={at > 0 ? () => setSelected(rows[at - 1]!.key) : undefined}
              onNext={at < rows.length - 1 ? () => setSelected(rows[at + 1]!.key) : undefined}
              onClose={() => setSelected(null)}
              onViewInContext={() => {
                pinned.current = false;
                setSelected(null);
                setReveal(current!.key);
              }}
            />
          </div>
        ) : null}
      </div>
    </div>
  );
}

function requestLevel(status: number): LogLevel {
  return status >= 500 ? "error" : status >= 400 ? "warn" : "info";
}

function requestDetail(r: Row): LogDetail {
  const { key, ...req } = r;
  return {
    t: r.t,
    level: requestLevel(r.status),
    message: `${r.method} ${r.path} ${r.status} ${formatMs(r.durationMs)}`,
    attributes: [
      { name: "host", value: r.host },
      { name: "path", value: r.path },
      { name: "method", value: r.method },
      { name: "status", value: String(r.status) },
      { name: "duration", value: formatMs(r.durationMs) },
      { name: "bytes", value: `${r.bytes} (${formatBytes(r.bytes)})` },
      { name: "client IP", value: r.clientIp },
      { name: "user agent", value: r.userAgent },
      { name: "upstream", value: r.upstream },
    ],
    raw: JSON.stringify(req, null, 2),
    line: JSON.stringify(req),
    metadata: [
      { name: "service", value: r.serviceName },
      { name: "deployment", value: r.deploymentId },
      { name: "timestamp", value: new Date(r.t).toISOString() },
    ],
    json: req,
  };
}
