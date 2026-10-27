import { clsx } from "clsx";
import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Check, Download, ExternalLink, Settings } from "@/components/ui/icons";
import { Menu, MenuItem } from "@/components/ui/Menu";
import { api } from "@/lib/api";
import {
  RETENTION_MS,
  formatLogDateTime,
  jsonAttributes,
  logLevel,
  logMessage,
  parseJsonLine,
  prettyRaw,
  timezoneLabel,
  type LogLevel,
} from "@/lib/logs";
import { LogDetailPane, type LogDetail } from "./LogDetailPane";
import { JumpButtons, LogSearch, atBottom, revealRow } from "./logParts";

interface Line {
  /** Stable key: live lines count up from 0, older ones down from -1. */
  id: number;
  ts: number;
  stderr: boolean;
  text: string;
  level: LogLevel;
}

type StreamState = "connecting" | "live" | "ended" | "error";

const MAX_LINES = 5000;
const OLDER_BATCH = 500;

const levelRow: Record<LogLevel, string> = {
  error: "border-error bg-error/6",
  warn: "border-warning bg-warning/6",
  info: "border-transparent",
};

/** Follows a deployment's container (or build) logs over SSE. Values of secret
 *  variables are already masked by the server. With `history`, older lines
 *  come from the environment's stored logs ("Load more"). */
export function LogViewer({
  url,
  downloadName = "logs.log",
  deploymentId,
  serviceName,
  history,
}: {
  url: string;
  downloadName?: string;
  deploymentId: string;
  serviceName?: string;
  /** Stored runtime logs to page back through (deploy logs only: build output isn't stored). */
  history?: { environmentId: string };
}) {
  const [lines, setLines] = useState<Line[]>([]);
  const [state, setState] = useState<StreamState>("connecting");
  const [message, setMessage] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [wrap, setWrap] = useState(true);
  const [showTime, setShowTime] = useState(true);
  const [selected, setSelected] = useState<number | null>(null);
  const [expanded, setExpanded] = useState(true);
  const [reveal, setReveal] = useState<number | null>(null);
  const [older, setOlder] = useState<{ loading: boolean; done: boolean; error: string | null }>({
    loading: false,
    done: false,
    error: null,
  });
  const box = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const pinned = useRef(true);
  const seq = useRef({ next: 0, prev: -1 });
  // Bumped to reconnect after the server restarted (an upgrade).
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let retry: ReturnType<typeof setTimeout> | undefined;
    setLines([]);
    setSelected(null);
    setOlder({ loading: false, done: false, error: null });
    seq.current = { next: 0, prev: -1 };
    pinned.current = true;
    setState("connecting");
    setMessage(null);
    const es = new EventSource(url, { withCredentials: true });
    es.onopen = () => setState("live");
    es.addEventListener("log", (e) => {
      const l = JSON.parse((e as MessageEvent<string>).data) as { ts: number; stderr: boolean; text: string };
      const line: Line = { ...l, id: seq.current.next++, level: logLevel(l) };
      setLines((prev) => (prev.length >= MAX_LINES ? [...prev.slice(-MAX_LINES + 1), line] : [...prev, line]));
    });
    es.addEventListener("error", (e) => {
      const data = (e as MessageEvent<string>).data;
      if (data) setMessage((JSON.parse(data) as { message: string }).message);
    });
    es.addEventListener("end", () => {
      setState("ended");
      es.close();
    });
    // The server is restarting (an upgrade): reconnect to the new one shortly.
    es.addEventListener("restart", () => {
      es.close();
      setState("connecting");
      retry = setTimeout(() => setAttempt((n) => n + 1), 1000);
    });
    es.onerror = () => {
      // A closed stream on a finished deployment is normal; anything else is shown.
      setState((s) => (s === "ended" || s === "connecting" ? s : "error"));
      es.close();
    };
    return () => {
      clearTimeout(retry);
      es.close();
    };
  }, [url, attempt]);

  useEffect(() => {
    if (pinned.current && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [lines, showTime, wrap]);

  // "View in context": scroll to the line once the list is visible again.
  useEffect(() => {
    if (reveal === null) return;
    revealRow(box.current, String(reveal));
    setReveal(null);
  }, [reveal]);

  const q = filter.trim().toLowerCase();
  const shown = useMemo(() => (q ? lines.filter((l) => l.text.toLowerCase().includes(q)) : lines), [lines, q]);

  const oldest = lines[0];
  // Lines past the retention aren't stored; asking for them would be refused.
  const canLoadOlder = !!history && !older.done && (!oldest || oldest.ts > Date.now() - RETENTION_MS + 60_000);

  const loadOlder = async () => {
    if (!history) return;
    setOlder((o) => ({ ...o, loading: true, error: null }));
    const until = oldest?.ts ?? Date.now();
    try {
      const found = await api.environmentLogs(history.environmentId, {
        since: until - RETENTION_MS,
        until,
        deployment: deploymentId,
        limit: OLDER_BATCH,
      });
      // Prepending must not yank the view: keep the same rows under the eye.
      const el = box.current;
      const fromBottom = el ? el.scrollHeight - el.scrollTop : 0;
      const got: Line[] = [];
      for (let i = found.lines.length - 1; i >= 0; i--) {
        const s = found.lines[i]!;
        const l = { ts: s.t, stderr: s.stream === "stderr", text: s.line };
        got.unshift({ ...l, id: seq.current.prev--, level: logLevel(l) });
      }
      setLines((prev) => [...got, ...prev]);
      setOlder({ loading: false, done: !found.truncated, error: null });
      requestAnimationFrame(() => {
        if (el) el.scrollTop = el.scrollHeight - fromBottom;
      });
    } catch (e) {
      setOlder({ loading: false, done: false, error: (e as Error).message });
    }
  };

  const fileBody = () => lines.map((l) => `${new Date(l.ts).toISOString()} ${l.text}`).join("\n") + "\n";
  const download = () => {
    const href = URL.createObjectURL(new Blob([fileBody()], { type: "text/plain" }));
    const a = document.createElement("a");
    a.href = href;
    a.download = downloadName;
    a.click();
    URL.revokeObjectURL(href);
  };
  // The loaded lines as plain text in a new tab (the SSE endpoint itself would
  // show event framing, not lines).
  const openWindow = () => {
    const href = URL.createObjectURL(new Blob([fileBody()], { type: "text/plain;charset=utf-8" }));
    window.open(href, "_blank", "noopener");
    setTimeout(() => URL.revokeObjectURL(href), 60_000);
  };

  const at = selected === null ? -1 : shown.findIndex((l) => l.id === selected);
  const current = at >= 0 ? shown[at]! : null;
  const detail = useMemo(() => (current ? lineDetail(current, deploymentId, serviceName) : null), [current, deploymentId, serviceName]);

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <div className="flex items-center gap-2">
        <LogSearch ref={search} value={filter} onChange={setFilter} placeholder="Filter and search logs" slashFocus />
        <span className="flex shrink-0 items-center gap-1.5 px-1 text-[13px] text-muted-foreground">
          <span
            className={clsx(
              "size-1.5 rounded-full",
              state === "live" ? "bg-success" : state === "error" ? "bg-error" : "bg-muted-foreground",
            )}
          />
          {state === "live" ? "Following" : state === "connecting" ? "Connecting…" : state === "ended" ? "Stream ended" : "Disconnected"}
        </span>
        <Button variant="ghost" size="icon" aria-label="Download logs" title="Download logs" disabled={lines.length === 0} onClick={download}>
          <Download />
        </Button>
        <Button variant="ghost" size="icon" aria-label="Open in new window" title="Open in new window" disabled={lines.length === 0} onClick={openWindow}>
          <ExternalLink />
        </Button>
      </div>
      {message ? <p className="text-sm text-error">{message}</p> : null}

      <div className="relative flex min-h-64 flex-1 flex-col">
        <div className="relative flex min-h-0 flex-1 flex-col overflow-hidden rounded-xl border border-border bg-background">
          <div className="flex h-8 shrink-0 items-center gap-3 border-b border-border px-3 font-mono text-[12.5px] text-muted-foreground">
            {showTime ? <span className="w-[19ch] shrink-0">Time ({timezoneLabel()})</span> : null}
            <span className="flex-1">Data</span>
            <Menu
              trigger={
                <Button variant="ghost" size="icon-xs" aria-label="Log settings" title="Log settings">
                  <Settings />
                </Button>
              }
            >
              <MenuItem icon={wrap ? <Check /> : <span className="size-4" />} onSelect={() => setWrap((w) => !w)}>
                Wrap lines
              </MenuItem>
              <MenuItem icon={showTime ? <Check /> : <span className="size-4" />} onSelect={() => setShowTime((s) => !s)}>
                Show timestamps
              </MenuItem>
            </Menu>
          </div>
          <div
            ref={box}
            onScroll={(e) => {
              pinned.current = atBottom(e.currentTarget);
            }}
            className="min-h-0 flex-1 overflow-auto py-1 font-mono text-[12.5px] leading-5"
            role="log"
            aria-live="off"
          >
            {history && (canLoadOlder || older.error) ? (
              <div className="flex items-center gap-2 px-3 py-1.5">
                <Button variant="outline" size="xs" loading={older.loading} onClick={loadOlder}>
                  Load more
                </Button>
                {older.error ? <span className="font-sans text-[13px] text-error">{older.error}</span> : null}
              </div>
            ) : history && older.done ? (
              <div className="px-3 py-1.5 font-sans text-[13px] text-muted-foreground">Start of the stored logs</div>
            ) : null}
            {shown.length === 0 ? (
              <div className="px-3 font-sans text-sm text-muted-foreground">
                {state === "connecting" ? "Loading logs…" : q ? "No lines match." : "No logs yet."}
              </div>
            ) : (
              shown.map((l) => (
                <div
                  key={l.id}
                  data-row={l.id}
                  onClick={() => {
                    if (window.getSelection()?.toString()) return;
                    setSelected(l.id);
                  }}
                  className={clsx(
                    "flex cursor-pointer gap-3 border-l-2 pr-3 pl-2.5 transition-colors hover:bg-accent/60",
                    levelRow[l.level],
                    l.id === selected && "bg-accent",
                  )}
                >
                  {showTime ? (
                    <span className="w-[19ch] shrink-0 text-muted-foreground tabular-nums select-none">{formatLogDateTime(l.ts)}</span>
                  ) : null}
                  <span className={clsx("min-w-0 flex-1", wrap ? "break-all whitespace-pre-wrap" : "whitespace-pre")}>{l.text}</span>
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
              onPrev={at > 0 ? () => setSelected(shown[at - 1]!.id) : undefined}
              onNext={at < shown.length - 1 ? () => setSelected(shown[at + 1]!.id) : undefined}
              onClose={() => setSelected(null)}
              onViewInContext={() => {
                pinned.current = false;
                setSelected(null);
                setReveal(current!.id);
              }}
            />
          </div>
        ) : null}
      </div>
    </div>
  );
}

function lineDetail(l: Line, deploymentId: string, serviceName?: string): LogDetail {
  const json = parseJsonLine(l.text);
  const stream = l.stderr ? "stderr" : "stdout";
  return {
    t: l.ts,
    level: l.level,
    message: logMessage(l.text, json),
    attributes: [{ name: "level", value: l.level }, { name: "stream", value: stream }, ...(json ? jsonAttributes(json) : [])],
    raw: prettyRaw(l.text),
    line: l.text,
    metadata: [
      ...(serviceName ? [{ name: "service", value: serviceName }] : []),
      { name: "deployment", value: deploymentId },
      { name: "timestamp", value: new Date(l.ts).toISOString() },
    ],
    json: { timestamp: new Date(l.ts).toISOString(), level: l.level, stream, message: json ?? l.text },
  };
}
