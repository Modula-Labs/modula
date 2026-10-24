import { useEffect, useRef, useState } from "react";
import type { MetricsRange } from "@/lib/api";
import { formatAxisTime, splitSegments, ticks, type XY } from "@/lib/metrics";

export interface Series {
  name: string;
  /** CSS color, e.g. "var(--chart-1)". */
  color: string;
  data: XY[];
}

const H = 140;
const PAD = { l: 74, r: 8, t: 6, b: 20 };

/** Hand-made SVG line chart: gridlines, x labels, hover crosshair + tooltip, gaps where data is missing. */
export function MetricsChart({
  title,
  latest,
  series,
  max,
  yLabel,
  valueLabel,
  range,
  from,
  to,
  maxGapMs,
  reference,
}: {
  title: string;
  latest: string;
  series: Series[];
  max: number;
  yLabel: (v: number) => string;
  valueLabel: (v: number) => string;
  range: MetricsRange;
  from: number;
  to: number;
  maxGapMs: number;
  reference?: { value: number; label: string };
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

  const iw = Math.max(w - PAD.l - PAD.r, 10);
  const ih = H - PAD.t - PAD.b;
  const span = Math.max(to - from, 1);
  const x = (t: number) => PAD.l + ((t - from) / span) * iw;
  const y = (v: number) => PAD.t + ih - (Math.min(v, max) / max) * ih;

  const xTicks = [0, 1, 2, 3].map((i) => from + (span * i) / 3);
  const timeline = Array.from(new Set(series.flatMap((s) => s.data.map((p) => p.t)))).sort((a, b) => a - b);

  const onMove = (e: React.PointerEvent<SVGSVGElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const t = from + ((e.clientX - r.left - PAD.l) / iw) * span;
    let best: number | null = null;
    for (const p of timeline) if (best === null || Math.abs(p - t) < Math.abs(best - t)) best = p;
    setHover(best);
  };

  const hoverX = hover === null ? 0 : x(hover);
  const tipLeft = hover !== null && hoverX > w * 0.6;

  return (
    <section className="rounded-xl border border-border bg-card p-4">
      <div className="mb-2 flex items-baseline justify-between gap-3">
        <h3 className="text-base leading-6 font-bold">{title}</h3>
        <div className="flex items-center gap-3 text-sm tabular-nums">
          {series.length > 1
            ? series.map((s) => (
                <span key={s.name} className="flex items-center gap-1.5 text-[13px] text-muted-foreground">
                  <span className="inline-block h-0.5 w-3" style={{ background: s.color }} />
                  {s.name}
                </span>
              ))
            : null}
          <span>{latest}</span>
        </div>
      </div>
      <div ref={wrap} className="relative">
        <svg
          width={w}
          height={H}
          role="img"
          aria-label={`${title} over the last ${range}`}
          onPointerMove={onMove}
          onPointerLeave={() => setHover(null)}
          className="block touch-pan-y select-none"
        >
          {ticks(max, 3).map((v) => (
            <g key={v}>
              <line x1={PAD.l} x2={w - PAD.r} y1={y(v)} y2={y(v)} stroke="var(--border)" strokeWidth={1} />
              <text x={PAD.l - 8} y={y(v) + 3.5} textAnchor="end" fontSize={11} fill="var(--muted-foreground)">
                {yLabel(v)}
              </text>
            </g>
          ))}
          {xTicks.map((t, i) => (
            <text
              key={i}
              x={x(t)}
              y={H - 4}
              textAnchor={i === 0 ? "start" : i === xTicks.length - 1 ? "end" : "middle"}
              fontSize={11}
              fill="var(--muted-foreground)"
            >
              {formatAxisTime(t, range)}
            </text>
          ))}
          {reference ? (
            <g>
              <line
                x1={PAD.l}
                x2={w - PAD.r}
                y1={y(reference.value)}
                y2={y(reference.value)}
                stroke="var(--muted-foreground)"
                strokeWidth={1}
                strokeDasharray="4 3"
              />
              <text x={w - PAD.r - 2} y={y(reference.value) - 4} textAnchor="end" fontSize={10} fill="var(--muted-foreground)">
                {reference.label}
              </text>
            </g>
          ) : null}
          {series.map((s) =>
            splitSegments(s.data, maxGapMs).map((seg, i) =>
              seg.length === 1 ? (
                <circle key={`${s.name}${i}`} cx={x(seg[0]!.t)} cy={y(seg[0]!.v)} r={2} fill={s.color} />
              ) : (
                <polyline
                  key={`${s.name}${i}`}
                  points={seg.map((p) => `${x(p.t).toFixed(1)},${y(p.v).toFixed(1)}`).join(" ")}
                  fill="none"
                  stroke={s.color}
                  strokeWidth={1.5}
                  strokeLinejoin="round"
                />
              ),
            ),
          )}
          {hover !== null ? (
            <g>
              <line x1={hoverX} x2={hoverX} y1={PAD.t} y2={PAD.t + ih} stroke="var(--muted-foreground)" strokeWidth={1} />
              {series.map((s) => {
                const p = s.data.find((d) => d.t === hover);
                return p ? <circle key={s.name} cx={hoverX} cy={y(p.v)} r={3} fill={s.color} stroke="var(--card)" strokeWidth={1.5} /> : null;
              })}
            </g>
          ) : null}
        </svg>
        {hover !== null ? (
          <div
            className="pointer-events-none absolute top-0 z-10 rounded-lg border border-border bg-popover px-2 py-1 text-xs shadow-raise"
            style={tipLeft ? { right: w - hoverX + 8 } : { left: hoverX + 8 }}
          >
            <div className="text-muted-foreground">{formatAxisTime(hover, range)}</div>
            {series.map((s) => {
              const p = s.data.find((d) => d.t === hover);
              return (
                <div key={s.name} className="flex items-center gap-1.5 tabular-nums">
                  <span className="inline-block h-0.5 w-3" style={{ background: s.color }} />
                  {series.length > 1 ? <span className="text-muted-foreground">{s.name}</span> : null}
                  {p ? valueLabel(p.v) : "—"}
                </div>
              );
            })}
          </div>
        ) : null}
      </div>
    </section>
  );
}
