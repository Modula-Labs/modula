import { clsx } from "clsx";
import { useEffect, useRef, useState } from "react";
import { Search } from "@/components/ui/icons";

interface Line {
  ts: number;
  stderr: boolean;
  text: string;
}

type StreamState = "connecting" | "live" | "ended" | "error";

const MAX_LINES = 5000;

/** Follows a deployment's container logs over SSE. Values of secret variables
 *  are already masked by the server. */
export function LogViewer({ url }: { url: string }) {
  const [lines, setLines] = useState<Line[]>([]);
  const [state, setState] = useState<StreamState>("connecting");
  const [message, setMessage] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const box = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  useEffect(() => {
    setLines([]);
    setState("connecting");
    setMessage(null);
    const es = new EventSource(url, { withCredentials: true });
    es.onopen = () => setState("live");
    es.addEventListener("log", (e) => {
      const line = JSON.parse((e as MessageEvent<string>).data) as Line;
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
    es.onerror = () => {
      // A closed stream on a finished deployment is normal; anything else is shown.
      setState((s) => (s === "ended" ? s : "error"));
      es.close();
    };
    return () => es.close();
  }, [url]);

  useEffect(() => {
    if (pinned.current && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [lines]);

  const q = filter.trim().toLowerCase();
  const shown = q ? lines.filter((l) => l.text.toLowerCase().includes(q)) : lines;

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <div className="flex items-center gap-3">
        <label className="flex h-9 flex-1 items-center gap-2 rounded-md border border-input px-2.5 text-muted-foreground focus-within:border-ring">
          <Search className="size-4" />
          <input
            className="min-w-0 flex-1 bg-transparent text-sm text-foreground outline-none placeholder:text-muted-foreground"
            placeholder="Filter logs"
            aria-label="Filter logs"
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
          />
        </label>
        <span className="text-xs text-muted-foreground">
          {state === "live" ? "Following" : state === "connecting" ? "Connecting…" : state === "ended" ? "Stream ended" : "Disconnected"}
        </span>
      </div>
      {message ? <p className="text-sm text-error">{message}</p> : null}
      <div
        ref={box}
        onScroll={(e) => {
          const el = e.currentTarget;
          pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
        }}
        className="min-h-64 flex-1 overflow-auto rounded-lg border border-border bg-background py-2 font-mono text-[12.5px] leading-5"
        role="log"
        aria-live="off"
      >
        {shown.length === 0 ? (
          <div className="px-3 text-muted-foreground">{state === "connecting" ? "Loading logs…" : "No logs yet."}</div>
        ) : (
          shown.map((l, i) => (
            <div key={i} className="flex gap-3 px-3 hover:bg-accent/60">
              <span className="shrink-0 text-muted-foreground tabular-nums select-none">
                {new Date(l.ts).toLocaleTimeString([], { hour12: false })}
              </span>
              <span className={clsx("break-all whitespace-pre-wrap", l.stderr && "text-foreground/85")}>{l.text}</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
