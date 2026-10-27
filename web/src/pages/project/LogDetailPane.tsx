import { clsx } from "clsx";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { ChevronDown, ChevronRight, ChevronLeft, Copy, DotsVertical, Maximize, Minimize, X } from "@/components/ui/icons";
import { Menu, MenuItem } from "@/components/ui/Menu";
import { Badge, type Tone } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import { formatLogDateTime, timezoneLabel, type LogLevel } from "@/lib/logs";

export interface Attribute {
  name: string;
  value: string;
}

/** One log line or request, as the detail pane shows it. */
export interface LogDetail {
  /** Unix ms. */
  t: number;
  level: LogLevel;
  /** Shown in the highlighted box at the top of Attributes. */
  message: string;
  attributes: Attribute[];
  /** The Raw Data tab, already pretty-printed. */
  raw: string;
  /** "Copy line": the line as it was written. */
  line: string;
  /** What we know about where it came from (service, deployment, …). */
  metadata: Attribute[];
  /** "Copy as JSON". */
  json: unknown;
}

const LEVELS: Record<LogLevel, { label: string; tone: Tone }> = {
  error: { label: "Error", tone: "danger" },
  warn: { label: "Warn", tone: "warn" },
  info: { label: "Info", tone: "neutral" },
};

const copy = (s: string) => void navigator.clipboard.writeText(s);

/** The pane a clicked log line opens in place of the list (Railway's layout):
 *  stepping through lines, the line's attributes and its raw data. Shared by
 *  deploy/build logs and network logs. */
export function LogDetailPane({
  detail,
  onPrev,
  onNext,
  onClose,
  onViewInContext,
  expanded,
  onToggleExpanded,
}: {
  detail: LogDetail;
  /** Undefined at the first/last line. */
  onPrev?: () => void;
  onNext?: () => void;
  onClose: () => void;
  onViewInContext: () => void;
  /** Expanded replaces the list; collapsed docks under it. */
  expanded: boolean;
  onToggleExpanded: () => void;
}) {
  const [tab, setTab] = useState<"attributes" | "raw">("attributes");
  const [metaOpen, setMetaOpen] = useState(false);
  const level = LEVELS[detail.level];

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-xl border border-border bg-background">
      <div className="flex items-center gap-1 border-b border-border px-2 py-1.5">
        <Button variant="ghost" size="icon-sm" aria-label="Previous line" title="Previous line" disabled={!onPrev} onClick={onPrev}>
          <ChevronLeft />
        </Button>
        <Button variant="ghost" size="icon-sm" aria-label="Next line" title="Next line" disabled={!onNext} onClick={onNext}>
          <ChevronRight />
        </Button>
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={expanded ? "Collapse" : "Expand"}
          title={expanded ? "Collapse" : "Expand"}
          onClick={onToggleExpanded}
        >
          {expanded ? <Minimize /> : <Maximize />}
        </Button>
        <div className="flex-1" />
        <Button variant="ghost" size="sm" onClick={onViewInContext}>
          View in context
        </Button>
        <Menu
          trigger={
            <Button variant="ghost" size="icon-sm" aria-label="More">
              <DotsVertical />
            </Button>
          }
        >
          <MenuItem icon={<Copy />} onSelect={() => copy(detail.line)}>
            Copy line
          </MenuItem>
          <MenuItem icon={<Copy />} onSelect={() => copy(JSON.stringify(detail.json, null, 2))}>
            Copy as JSON
          </MenuItem>
        </Menu>
        <Button variant="ghost" size="icon-sm" aria-label="Close" title="Close" onClick={onClose}>
          <X />
        </Button>
      </div>

      <div className="min-h-0 flex-1 overflow-auto px-4 py-4">
        <div className="flex flex-wrap items-center gap-2.5">
          <span className="font-mono text-lg tabular-nums">{formatLogDateTime(detail.t)}</span>
          <span className="text-sm text-muted-foreground">{timezoneLabel()}</span>
          <Badge tone={level.tone}>{level.label}</Badge>
        </div>
        <div className="mt-4">
          <Segmented
            label="Detail view"
            value={tab}
            onChange={setTab}
            options={[
              { value: "attributes", label: "Attributes" },
              { value: "raw", label: "Raw Data" },
            ]}
          />
        </div>

        {tab === "attributes" ? (
          <div className="mt-4 flex flex-col gap-4">
            <div
              className={clsx(
                "relative rounded-lg border py-2 pr-10 pl-3 font-mono text-[12.5px] leading-5 break-words whitespace-pre-wrap",
                detail.level === "error"
                  ? "border-error/30 bg-error/8 text-error"
                  : detail.level === "warn"
                    ? "border-warning/30 bg-warning/8 text-warning"
                    : "border-border bg-muted",
              )}
            >
              {detail.message || <span className="text-muted-foreground">(empty line)</span>}
              <div className="absolute top-1 right-1">
                <CopyButton value={detail.message} label="Copy message" />
              </div>
            </div>
            <AttributeTable rows={detail.attributes} />
            <div>
              <button
                type="button"
                aria-expanded={metaOpen}
                onClick={() => setMetaOpen((o) => !o)}
                className="flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground"
              >
                {metaOpen ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}
                Modula metadata
              </button>
              {metaOpen ? (
                <div className="mt-2">
                  <AttributeTable rows={detail.metadata} />
                </div>
              ) : null}
            </div>
          </div>
        ) : (
          <div className="relative mt-4 rounded-lg border border-border bg-muted py-2 pr-10 pl-3">
            <pre className="overflow-x-auto font-mono text-[12.5px] leading-5 break-words whitespace-pre-wrap">{detail.raw}</pre>
            <div className="absolute top-1 right-1">
              <CopyButton value={detail.raw} label="Copy raw data" />
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

function AttributeTable({ rows }: { rows: Attribute[] }) {
  return (
    <table className="w-full table-fixed text-[13px]">
      <thead>
        <tr className="text-left text-muted-foreground">
          <th className="w-40 pb-1.5 font-normal">Name</th>
          <th className="pb-1.5 font-normal">Value</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((r, i) => (
          <tr key={`${i}-${r.name}`} className="align-top hover:bg-accent/60">
            <td className="truncate py-1 pr-3 font-mono text-muted-foreground" title={r.name}>
              {r.name}
            </td>
            <td className="py-1 font-mono break-words whitespace-pre-wrap">{r.value || <span className="text-muted-foreground">—</span>}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
