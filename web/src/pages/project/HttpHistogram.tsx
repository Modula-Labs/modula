import { useEffect, useRef, useState } from "react";
import type { HttpBucket } from "@/lib/api";
import { httpBucketWindow } from "@/lib/http";
import { formatWindowTime } from "@/lib/logs";

const H = 72;
const PAD = { t: 4, b: 18 };

/** Hand-made SVG bar chart of request volume over the window, stacked as ok / 4xx / 5xx.
 *  Clicking a bar asks to narrow the window to it. */
export function HttpHistogram({
  buckets,
  bucketMs,
  since,
  until,
  onSelect,
}: {
  buckets: HttpBucket[];
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
        aria-label="Requests over time. Click a bar to narrow the time range."
        className="block cursor-pointer select-none"
        onPointerLeave={() => setHover(null)}
      >
        <line x1={0} x2={iw} y1={PAD.t + ih} y2={PAD.t + ih} stroke="var(--border)" />
        {buckets.map((b, i) => {
          const base = PAD.t + ih;
          const h5 = h(b.status5xx);
          const h4 = h(b.status4xx);
          const hOk = h(Math.max(b.count - b.status4xx - b.status5xx, 0));
          const bx = x(b.t);
          const op = hover === i ? 1 : 0.8;
          return (
            <g key={b.t} onPointerEnter={() => setHover(i)} onClick={() => onSelect(httpBucketWindow(b, bucketMs, until))}>
              {/* A full-height hit area so tiny bars are still easy to hit. */}
              <rect x={bx} y={PAD.t} width={bw} height={ih} fill="transparent" />
              {hOk > 0 ? <rect x={bx} y={base - h5 - h4 - hOk} width={bw} height={hOk} rx={1} fill="var(--chart-3)" opacity={op - 0.05} /> : null}
              {h4 > 0 ? <rect x={bx} y={base - h5 - h4} width={bw} height={h4} rx={1} fill="var(--warning)" opacity={op} /> : null}
              {h5 > 0 ? <rect x={bx} y={base - h5} width={bw} height={h5} rx={1} fill="var(--destructive)" opacity={op} /> : null}
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
          className="pointer-events-none absolute top-1 z-10 rounded-lg border border-border bg-popover px-2 py-1 text-xs shadow-raise"
          style={hoverX > w * 0.6 ? { right: w - hoverX - 4 } : { left: hoverX + 8 }}
        >
          <div className="text-muted-foreground">{formatWindowTime(hb.t, span)}</div>
          <div className="tabular-nums">
            {hb.count} {hb.count === 1 ? "request" : "requests"}
            {hb.status4xx > 0 ? <span className="text-warning"> · {hb.status4xx} 4xx</span> : null}
            {hb.status5xx > 0 ? <span className="text-error"> · {hb.status5xx} 5xx</span> : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}
