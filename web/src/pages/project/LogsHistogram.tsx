import { useEffect, useRef, useState } from "react";
import type { LogBucket } from "@/lib/api";
import { bucketWindow, formatWindowTime } from "@/lib/logs";

const H = 72;
const PAD = { t: 4, b: 18 };

/** Hand-made SVG bar chart of log volume over the window; stderr counts are stacked in the error tint.
 *  Clicking a bar asks to narrow the window to it. */
export function LogsHistogram({
  buckets,
  bucketMs,
  since,
  until,
  onSelect,
}: {
  buckets: LogBucket[];
  bucketMs: number;
  since: number;
  until: number;
  onSelect: (w: { since: number; until: number }) => void;
}) {
  const wrap = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(600);
  const [hover, setHover] = useState<number | null>(null);

  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    setW(el.clientWidth || 600);
    const ro = new ResizeObserver(() => setW(el.clientWidth || 600));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const iw = Math.max(w - 24, 10); // the card has 12px padding each side
  const span = Math.max(until - since, 1);
  const max = Math.max(1, ...buckets.map((b) => b.count));
  const ih = H - PAD.t - PAD.b;
  const x = (t: number) => ((t - since) / span) * iw;
  const bw = Math.max((bucketMs / span) * iw - 1, 1);
  const h = (n: number) => (n / max) * ih;
  const hb = hover === null ? undefined : buckets[hover];
  const hoverX = hb ? x(hb.t) + 12 + bw / 2 : 0;

  return (
    <div ref={wrap} className="relative rounded-xl border border-border bg-card px-3 pt-1">
      <svg
        width={iw}
        height={H}
        role="img"
        aria-label="Log volume over time. Click a bar to narrow the time range."
        className="block cursor-pointer select-none"
        onPointerLeave={() => setHover(null)}
      >
        <line x1={0} x2={iw} y1={PAD.t + ih} y2={PAD.t + ih} stroke="var(--border)" />
        {buckets.map((b, i) => {
          const base = PAD.t + ih;
          const errH = h(b.errors);
          const okH = h(b.count - b.errors);
          const bx = x(b.t);
          const bww = bw;
          return (
            <g key={b.t} onPointerEnter={() => setHover(i)} onClick={() => onSelect(bucketWindow(b, bucketMs, until))}>
              {/* A full-height hit area so tiny bars are still easy to hit. */}
              <rect x={bx} y={PAD.t} width={bww} height={ih} fill="transparent" />
              {okH > 0 ? <rect x={bx} y={base - errH - okH} width={bww} height={okH} rx={1} fill="var(--chart-3)" opacity={hover === i ? 1 : 0.75} /> : null}
              {errH > 0 ? <rect x={bx} y={base - errH} width={bww} height={errH} rx={1} fill="var(--destructive)" opacity={hover === i ? 1 : 0.85} /> : null}
            </g>
          );
        })}
        <text x={0} y={H - 4} fontSize={11} fill="var(--muted-foreground)">
          {formatWindowTime(since, span)}
        </text>
        <text x={iw} y={H - 4} textAnchor="end" fontSize={11} fill="var(--muted-foreground)">
          {formatWindowTime(until, span)}
        </text>
      </svg>
      {hb ? (
        <div
          className="pointer-events-none absolute top-1 z-10 rounded-md border border-border bg-popover px-2 py-1 text-xs shadow-raise"
          style={hoverX > w * 0.6 ? { right: w - hoverX - 4 } : { left: hoverX + 8 }}
        >
          <div className="text-muted-foreground">{formatWindowTime(hb.t, span)}</div>
          <div className="tabular-nums">
            {hb.count} {hb.count === 1 ? "line" : "lines"}
            {hb.errors > 0 ? <span className="text-error"> · {hb.errors} stderr</span> : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}
