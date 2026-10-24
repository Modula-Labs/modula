import { clsx } from "clsx";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Switch } from "@/components/ui/Controls";
import { Input, Select, Textarea } from "@/components/ui/Input";
import { ChevronDown, ChevronUp, GripVertical, Plus, Trash, X } from "@/components/ui/icons";
import { Modal, MODAL_WIDTHS } from "@/components/ui/Modal";
import { Alert, Badge } from "@/components/ui/misc";
import type { EdgeAction, EdgeCondition, EdgeRule } from "@/lib/api";
import {
  ACTION_TYPES,
  CONDITION_FIELDS,
  MAX_AND_CONDITIONS,
  MAX_EDGE_RULES,
  MAX_OR_GROUPS,
  REDIRECT_STATUSES,
  cleanRules,
  defaultAction,
  defaultCondition,
  describeAction,
  moveItem,
  newRule,
  parseList,
  parseRulesJson,
  rulesToJson,
  validateRules,
  type ActionType,
  type ConditionField,
} from "@/lib/edge";
import { HTTP_METHODS } from "@/lib/http";

interface Draft {
  id: string;
  rule: EdgeRule;
}

let nextId = 0;
const draftOf = (rule: EdgeRule): Draft => ({ id: `edge-rule-${++nextId}`, rule });

const fixed = (w: string, children: ReactNode) => <div className={clsx("shrink-0", w)}>{children}</div>;

/** A number field that lets the text be empty while typing; an empty field is NaN, which validation rejects. */
function NumInput({ value, onChange, className, label }: { value: number; onChange: (n: number) => void; className?: string; label: string }) {
  const show = (n: number) => (Number.isFinite(n) ? String(n) : "");
  const parse = (s: string) => (s.trim() === "" ? NaN : Number(s));
  const [text, setText] = useState(show(value));
  useEffect(() => {
    const p = parse(text);
    if (p !== value && !(Number.isNaN(p) && Number.isNaN(value))) setText(show(value));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value]);
  return (
    <Input
      aria-label={label}
      className={className}
      inputMode="numeric"
      value={text}
      onChange={(e) => {
        setText(e.target.value);
        onChange(parse(e.target.value));
      }}
    />
  );
}

/** A comma-separated list field; the text stays as typed while the list is parsed from it. */
function ListInput({ value, onChange, placeholder, label, className }: { value: string[]; onChange: (v: string[]) => void; placeholder?: string; label: string; className?: string }) {
  const [text, setText] = useState(value.join(", "));
  useEffect(() => {
    if (parseList(text).join("\n") !== value.join("\n")) setText(value.join(", "));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [value]);
  return (
    <Input
      mono
      aria-label={label}
      className={className}
      placeholder={placeholder}
      value={text}
      onChange={(e) => {
        setText(e.target.value);
        onChange(parseList(e.target.value));
      }}
    />
  );
}

function ConditionRow({ c, onChange, onRemove }: { c: EdgeCondition; onChange: (c: EdgeCondition) => void; onRemove: () => void }) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Button
        size="md"
        variant="outline"
        aria-pressed={!!c.negate}
        title="Match when the condition is false"
        className={clsx(c.negate && "bg-accent")}
        onClick={() => onChange({ ...c, negate: c.negate ? undefined : true })}
      >
        Not
      </Button>
      {fixed(
        "w-32",
        <Select aria-label="Field" value={c.field} onChange={(e) => onChange(defaultCondition(e.target.value as ConditionField, c.negate))}>
          {CONDITION_FIELDS.map((f) => (
            <option key={f.value} value={f.value}>
              {f.label}
            </option>
          ))}
        </Select>,
      )}
      {c.field === "path" ? (
        <>
          {fixed(
            "w-28",
            <Select aria-label="Operator" value={c.op} onChange={(e) => onChange({ ...c, op: e.target.value as typeof c.op })}>
              <option value="matches">matches</option>
              <option value="prefix">starts with</option>
              <option value="equals">equals</option>
            </Select>,
          )}
          <Input mono aria-label="Path" className="min-w-40 flex-1" placeholder="/admin*" value={c.value} onChange={(e) => onChange({ ...c, value: e.target.value })} />
        </>
      ) : null}
      {c.field === "method" ? (
        <>
          <span className="text-sm text-muted-foreground">is one of</span>
          <div className="flex flex-wrap gap-1.5" role="group" aria-label="Methods">
            {HTTP_METHODS.map((m) => {
              const on = c.values.includes(m);
              return (
                <Button
                  key={m}
                  size="sm"
                  variant="outline"
                  aria-pressed={on}
                  className={clsx("font-mono text-xs", on && "bg-accent font-bold")}
                  onClick={() => onChange({ ...c, values: on ? c.values.filter((v) => v !== m) : [...c.values, m] })}
                >
                  {m}
                </Button>
              );
            })}
          </div>
        </>
      ) : null}
      {c.field === "header" ? (
        <>
          <Input mono aria-label="Header name" className="w-40" placeholder="X-Api-Key" value={c.name} onChange={(e) => onChange({ ...c, name: e.target.value })} />
          {fixed(
            "w-28",
            <Select
              aria-label="Operator"
              value={c.op}
              onChange={(e) => {
                const op = e.target.value as typeof c.op;
                onChange(op === "exists" ? { field: "header", name: c.name, op, negate: c.negate } : { ...c, op, value: c.value ?? "" });
              }}
            >
              <option value="exists">exists</option>
              <option value="equals">equals</option>
              <option value="contains">contains</option>
            </Select>,
          )}
          {c.op !== "exists" ? (
            <Input mono aria-label="Header value" className="min-w-32 flex-1" placeholder="value" value={c.value ?? ""} onChange={(e) => onChange({ ...c, value: e.target.value })} />
          ) : null}
        </>
      ) : null}
      {c.field === "ip" ? (
        <>
          <span className="text-sm text-muted-foreground">in</span>
          <ListInput label="IP addresses or ranges" className="min-w-48 flex-1" placeholder="203.0.113.0/24, 2001:db8::/32" value={c.cidrs} onChange={(cidrs) => onChange({ ...c, cidrs })} />
        </>
      ) : null}
      {c.field === "country" ? (
        <>
          <span className="text-sm text-muted-foreground">in</span>
          <ListInput label="Country codes" className="min-w-48 flex-1" placeholder="US, DE" value={c.values} onChange={(values) => onChange({ ...c, values })} />
        </>
      ) : null}
      <Button size="icon" variant="ghost" aria-label="Remove condition" onClick={onRemove}>
        <X />
      </Button>
    </div>
  );
}

function WhenEditor({ rule, onChange }: { rule: EdgeRule; onChange: (r: EdgeRule) => void }) {
  const groups = rule.when;
  const setGroups = (when: EdgeCondition[][]) => onChange({ ...rule, when: when.filter((g) => g.length > 0) });
  const hasCountry = groups.some((g) => g.some((c) => c.field === "country"));
  return (
    <div className="flex min-w-0 flex-1 flex-col gap-2">
      {groups.length === 0 ? (
        <div className="flex items-center gap-3">
          <span className="text-sm text-muted-foreground">Every request</span>
          <Button size="sm" variant="outline" icon={<Plus />} onClick={() => setGroups([[defaultCondition("path")]])}>
            Condition
          </Button>
        </div>
      ) : (
        groups.map((g, gi) => (
          <div key={gi} className="flex flex-col gap-2">
            {gi > 0 ? (
              <div className="flex items-center gap-2 text-xs text-muted-foreground" aria-hidden>
                <span className="h-px flex-1 bg-border" />
                or
                <span className="h-px flex-1 bg-border" />
              </div>
            ) : null}
            {g.map((c, ci) => (
              <ConditionRow
                key={ci}
                c={c}
                onChange={(next) => setGroups(groups.map((x, i) => (i === gi ? x.map((y, j) => (j === ci ? next : y)) : x)))}
                onRemove={() => setGroups(groups.map((x, i) => (i === gi ? x.filter((_, j) => j !== ci) : x)))}
              />
            ))}
            <div className="flex gap-2">
              <Button
                size="sm"
                variant="outline"
                icon={<Plus />}
                disabled={g.length >= MAX_AND_CONDITIONS}
                onClick={() => setGroups(groups.map((x, i) => (i === gi ? [...x, defaultCondition("path")] : x)))}
              >
                And
              </Button>
              {gi === groups.length - 1 ? (
                <Button size="sm" variant="outline" icon={<Plus />} disabled={groups.length >= MAX_OR_GROUPS} onClick={() => setGroups([...groups, [defaultCondition("path")]])}>
                  Or
                </Button>
              ) : null}
            </div>
          </div>
        ))
      )}
      {hasCountry ? <p className="text-[13px] text-muted-foreground">Country conditions only work on Cloudflare-proxied domains.</p> : null}
    </div>
  );
}

const WINDOW_PRESETS = [
  { secs: 1, label: "1s" },
  { secs: 10, label: "10s" },
  { secs: 60, label: "1m" },
  { secs: 3600, label: "1h" },
];

function HeadersEditor({ action, onChange }: { action: Extract<EdgeAction, { type: "headers" }>; onChange: (a: EdgeAction) => void }) {
  // Rows can be half-filled while typing, which a record can't hold, so they live here and the record is derived.
  const [rows, setRows] = useState(() => Object.entries(action.set ?? {}).map(([k, v]) => ({ k, v })));
  const commit = (next: { k: string; v: string }[], remove = action.remove) => {
    setRows(next);
    const set = Object.fromEntries(next.filter((r) => r.k.trim()).map((r) => [r.k.trim(), r.v]));
    onChange({ type: "headers", ...(Object.keys(set).length ? { set } : {}), ...(remove?.length ? { remove } : {}) });
  };
  return (
    <div className="flex min-w-0 flex-1 flex-col gap-2">
      {rows.map((r, i) => (
        <div key={i} className="flex items-center gap-2">
          <Input mono aria-label="Header name" className="w-44" placeholder="X-Frame-Options" value={r.k} onChange={(e) => commit(rows.map((x, j) => (j === i ? { ...x, k: e.target.value } : x)))} />
          <Input mono aria-label="Header value" className="min-w-0 flex-1" placeholder="DENY" value={r.v} onChange={(e) => commit(rows.map((x, j) => (j === i ? { ...x, v: e.target.value } : x)))} />
          <Button size="icon" variant="ghost" aria-label="Remove header" onClick={() => commit(rows.filter((_, j) => j !== i))}>
            <X />
          </Button>
        </div>
      ))}
      <div>
        <Button size="sm" variant="outline" icon={<Plus />} onClick={() => setRows((r) => [...r, { k: "", v: "" }])}>
          Set header
        </Button>
      </div>
      <div className="flex items-center gap-2">
        <span className="shrink-0 text-sm text-muted-foreground">Remove</span>
        <ListInput label="Headers to remove" className="min-w-0 flex-1" placeholder="Server, X-Powered-By" value={action.remove ?? []} onChange={(remove) => commit(rows, remove)} />
      </div>
      <p className="text-[13px] text-muted-foreground">Applies to the response sent to the client.</p>
    </div>
  );
}

function ActionFields({ action, onChange }: { action: EdgeAction; onChange: (a: EdgeAction) => void }) {
  const [bodyOpen, setBodyOpen] = useState(action.type === "block" && !!action.body);
  switch (action.type) {
    case "rateLimit":
      return (
        <div className="flex min-w-0 flex-1 flex-col gap-2">
          <div className="flex flex-wrap items-center gap-2">
            <NumInput label="Requests" className="w-24" value={action.requests} onChange={(requests) => onChange({ ...action, requests })} />
            <span className="text-sm text-muted-foreground">requests per</span>
            <NumInput label="Window in seconds" className="w-20" value={action.windowSecs} onChange={(windowSecs) => onChange({ ...action, windowSecs })} />
            <span className="text-sm text-muted-foreground">seconds</span>
            {WINDOW_PRESETS.map((p) => (
              <Button key={p.secs} size="sm" variant="outline" aria-pressed={action.windowSecs === p.secs} className={clsx(action.windowSecs === p.secs && "bg-accent")} onClick={() => onChange({ ...action, windowSecs: p.secs })}>
                {p.label}
              </Button>
            ))}
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm text-muted-foreground">Count by</span>
            {fixed(
              "w-28",
              <Select
                aria-label="Count by"
                value={action.key}
                onChange={(e) => {
                  const { header: _h, ...rest } = action;
                  onChange(e.target.value === "header" ? { ...rest, key: "header", header: "" } : { ...rest, key: "ip" });
                }}
              >
                <option value="ip">Client IP</option>
                <option value="header">Header</option>
              </Select>,
            )}
            {action.key === "header" ? (
              <Input mono aria-label="Header name" className="w-44" placeholder="X-Api-Key" value={action.header ?? ""} onChange={(e) => onChange({ ...action, header: e.target.value })} />
            ) : null}
          </div>
          <p className="text-[13px] text-muted-foreground">Over the limit, requests get a 429. Limits are counted per server.</p>
        </div>
      );
    case "block":
      return (
        <div className="flex min-w-0 flex-1 flex-col gap-2">
          <div className="flex flex-wrap items-center gap-2">
            <NumInput label="Status code" className="w-20" value={action.status} onChange={(status) => onChange({ ...action, status })} />
            {!bodyOpen ? (
              <Button size="sm" variant="ghost" icon={<Plus />} onClick={() => setBodyOpen(true)}>
                Response body
              </Button>
            ) : null}
          </div>
          {bodyOpen ? (
            <Textarea
              aria-label="Response body"
              placeholder="Response body (up to 4096 bytes)"
              value={action.body ?? ""}
              onChange={(e) => onChange({ ...action, body: e.target.value })}
            />
          ) : null}
        </div>
      );
    case "redirect":
      return (
        <div className="flex min-w-0 flex-1 flex-col gap-2">
          <div className="flex flex-wrap items-center gap-2">
            {fixed(
              "w-20",
              <Select aria-label="Redirect status" value={action.status} onChange={(e) => onChange({ ...action, status: Number(e.target.value) as typeof action.status })}>
                {REDIRECT_STATUSES.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </Select>,
            )}
            <Input mono aria-label="Redirect target" className="min-w-48 flex-1" placeholder="https://{host}/new{path}" value={action.target} onChange={(e) => onChange({ ...action, target: e.target.value })} />
          </div>
          <p className="text-[13px] text-muted-foreground">
            A path or an https:// URL. Placeholders: <code className="font-mono">{"{host}"}</code> <code className="font-mono">{"{path}"}</code> <code className="font-mono">{"{query}"}</code>
          </p>
        </div>
      );
    case "headers":
      return <HeadersEditor action={action} onChange={onChange} />;
    case "cache":
      return (
        <div className="flex min-w-0 flex-1 flex-col gap-2">
          <div className="flex items-center gap-2">
            <NumInput label="Cache time in seconds" className="w-28" value={action.ttlSecs} onChange={(ttlSecs) => onChange({ ...action, ttlSecs })} />
            <span className="text-sm text-muted-foreground">seconds</span>
          </div>
          <p className="text-[13px] text-muted-foreground">Requests with cookies or an Authorization header are never cached, and no-cache is honoured.</p>
        </div>
      );
    case "allow":
      return <p className="self-center text-sm text-muted-foreground">Skips the remaining rules and sends the request to your service.</p>;
  }
}

function RuleCard({
  d,
  index,
  errors,
  collapsed,
  dragging,
  dropTarget,
  onToggle,
  onChange,
  onRemove,
  onMove,
  onDragStart,
  onDragEnd,
  onDragOver,
  onDrop,
}: {
  d: Draft;
  index: number;
  errors: string[];
  collapsed: boolean;
  dragging: boolean;
  dropTarget: boolean;
  onToggle: () => void;
  onChange: (r: EdgeRule) => void;
  onRemove: () => void;
  onMove: (delta: -1 | 1) => void;
  onDragStart: () => void;
  onDragEnd: () => void;
  onDragOver: () => void;
  onDrop: () => void;
}) {
  const r = d.rule;
  const card = useRef<HTMLDivElement>(null);
  return (
    <div
      ref={card}
      onDragOver={(e) => {
        e.preventDefault();
        onDragOver();
      }}
      onDrop={(e) => {
        e.preventDefault();
        onDrop();
      }}
      className={clsx("rounded-xl border bg-card", errors.length ? "border-destructive/60" : "border-border", dragging && "opacity-40", dropTarget && "ring-2 ring-foreground/30")}
    >
      <div className="flex items-center gap-2 px-2 py-2">
        <button
          type="button"
          draggable
          data-edge-handle={d.id}
          aria-label={`Reorder rule ${index + 1}. Drag, or press the up and down arrow keys.`}
          title="Drag to reorder"
          className="flex size-7 shrink-0 cursor-grab items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-foreground/7 focus-visible:ring-3 focus-visible:ring-ring/50 active:cursor-grabbing"
          onDragStart={(e) => {
            e.dataTransfer.effectAllowed = "move";
            e.dataTransfer.setData("text/plain", d.id);
            if (card.current) e.dataTransfer.setDragImage(card.current, 16, 16);
            onDragStart();
          }}
          onDragEnd={onDragEnd}
          onKeyDown={(e) => {
            if (e.key === "ArrowUp" || e.key === "ArrowDown") {
              e.preventDefault();
              onMove(e.key === "ArrowUp" ? -1 : 1);
            }
          }}
        >
          <GripVertical className="size-4" />
        </button>
        <span className="w-4 shrink-0 text-sm text-muted-foreground tabular-nums">{index + 1}</span>
        <Input
          aria-label={`Name of rule ${index + 1}`}
          className="min-w-0 flex-1"
          placeholder="New rule"
          value={r.name}
          onChange={(e) => onChange({ ...r, name: e.target.value })}
        />
        <Badge>{describeAction(r.then)}</Badge>
        <Switch aria-label={`Rule ${index + 1} enabled`} title={r.enabled ? "Enabled" : "Disabled: kept but not applied"} checked={r.enabled} onCheckedChange={(enabled) => onChange({ ...r, enabled })} />
        <Button size="icon" variant="ghost" aria-label={`Delete rule ${index + 1}`} onClick={onRemove}>
          <Trash />
        </Button>
        <Button size="icon" variant="ghost" aria-label={collapsed ? "Expand rule" : "Collapse rule"} aria-expanded={!collapsed} onClick={onToggle}>
          {collapsed ? <ChevronDown /> : <ChevronUp />}
        </Button>
      </div>
      {!collapsed ? (
        <div className={clsx("flex flex-col gap-3 border-t border-border px-3 py-3", !r.enabled && "opacity-70")}>
          <div className="flex gap-3">
            <span className="w-11 shrink-0 pt-1.5 text-sm text-muted-foreground">When</span>
            <WhenEditor rule={r} onChange={onChange} />
          </div>
          <div className="flex gap-3">
            <span className="w-11 shrink-0 pt-1.5 text-sm text-muted-foreground">Then</span>
            <div className="flex min-w-0 flex-1 gap-2">
              {fixed(
                "w-36 self-start",
                <Select aria-label="Action" value={r.then.type} onChange={(e) => onChange({ ...r, then: defaultAction(e.target.value as ActionType) })}>
                  {ACTION_TYPES.map((a) => (
                    <option key={a.value} value={a.value}>
                      {a.label}
                    </option>
                  ))}
                </Select>,
              )}
              <ActionFields action={r.then} onChange={(then) => onChange({ ...r, then })} />
            </div>
          </div>
        </div>
      ) : null}
      {errors.length ? (
        <ul className="flex flex-col gap-0.5 border-t border-border px-3 py-2 text-sm text-error">
          {errors.map((e) => (
            <li key={e}>{e}</li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

const Connector = () => <div aria-hidden className="mx-auto h-4 w-px bg-border" />;
const Pill = ({ children }: { children: ReactNode }) => (
  <div className="mx-auto rounded-full border border-border bg-muted px-3 py-0.5 text-[13px] text-muted-foreground">{children}</div>
);

/** The flow editor for a service's edge rules: reorder, edit conditions and actions, or edit the JSON.
 *  `onSave` resolves once the rules are stored; a rejection's message is shown in the dialog. */
export function EdgeRulesDialog({
  open,
  onOpenChange,
  rules,
  onSave,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  rules: EdgeRule[];
  onSave: (rules: EdgeRule[]) => Promise<void>;
}) {
  const [drafts, setDrafts] = useState<Draft[]>([]);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [jsonMode, setJsonMode] = useState(false);
  const [jsonText, setJsonText] = useState("");
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [drag, setDrag] = useState<{ from: number; over: number | null } | null>(null);
  const focusAfter = useRef<string | null>(null);

  // Every time the dialog opens it starts from the saved rules.
  useEffect(() => {
    if (!open) return;
    const ds = rules.map(draftOf);
    setDrafts(ds);
    setCollapsed(new Set(ds.length > 1 ? ds.map((d) => d.id) : []));
    setJsonMode(false);
    setConfirmDiscard(false);
    setError(null);
    setSaving(false);
    setDrag(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!focusAfter.current) return;
    document.querySelector<HTMLElement>(`[data-edge-handle="${focusAfter.current}"]`)?.focus();
    focusAfter.current = null;
  }, [drafts]);

  const parsed = useMemo(() => (jsonMode ? parseRulesJson(jsonText) : null), [jsonMode, jsonText]);
  const current: EdgeRule[] | null = jsonMode ? (parsed?.ok ? parsed.rules : null) : drafts.map((d) => d.rule);
  const check = useMemo(() => (current ? validateRules(current) : null), [current]);
  const initialJson = useMemo(() => JSON.stringify(cleanRules(rules)), [rules]);
  const dirty = current === null || JSON.stringify(cleanRules(current)) !== initialJson;
  const jsonErrors = check ? [check.general, ...check.rules.flatMap((e, i) => e.map((m) => `Rule ${i + 1}: ${m}`))].filter((x): x is string => !!x) : [];
  const canSave = !!current && !!check?.valid && dirty && !saving;

  const requestClose = () => {
    if (saving) return;
    if (dirty) setConfirmDiscard(true);
    else onOpenChange(false);
  };

  const setRule = (id: string, rule: EdgeRule) => setDrafts((ds) => ds.map((d) => (d.id === id ? { ...d, rule } : d)));
  const move = (from: number, to: number, focus?: string) => {
    if (focus) focusAfter.current = focus;
    setDrafts((ds) => moveItem(ds, from, to));
  };
  const addRule = () => {
    const d = draftOf(newRule());
    setDrafts((ds) => [...ds, d]);
  };
  const toggleJson = () => {
    if (!jsonMode) {
      setJsonText(rulesToJson(drafts.map((d) => d.rule)));
      setJsonMode(true);
      return;
    }
    if (parsed?.ok) {
      const ds = parsed.rules.map(draftOf);
      setDrafts(ds);
      setCollapsed(new Set(ds.length > 1 ? ds.map((d) => d.id) : []));
      setJsonMode(false);
    }
  };

  const save = async () => {
    if (!current) return;
    setSaving(true);
    setError(null);
    try {
      await onSave(cleanRules(current));
      onOpenChange(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Couldn't save the rules");
    } finally {
      setSaving(false);
    }
  };

  const atLimit = drafts.length >= MAX_EDGE_RULES;
  const count = current?.length ?? drafts.length;

  return (
    <Modal
      open={open}
      onOpenChange={(o) => (o ? onOpenChange(true) : requestClose())}
      title="Edge Rules"
      description="Rules run top to bottom on every request to this service's domains. The first matching rule with a terminal action decides the outcome."
      width={MODAL_WIDTHS.lg}
    >
      <div className="flex flex-col gap-4">
        {jsonMode ? (
          <div className="flex flex-col gap-2">
            <Textarea
              aria-label="Edge rules as JSON"
              spellCheck={false}
              className="min-h-72 text-[12.5px] leading-5"
              value={jsonText}
              onChange={(e) => setJsonText(e.target.value)}
              aria-invalid={parsed ? !parsed.ok : false}
            />
            {parsed && !parsed.ok ? <p className="text-sm text-error">{parsed.error}</p> : null}
            {jsonErrors.length ? (
              <ul className="flex flex-col gap-0.5 text-sm text-error">
                {jsonErrors.map((m) => (
                  <li key={m}>{m}</li>
                ))}
              </ul>
            ) : null}
          </div>
        ) : (
          <div className="flex flex-col" role="list" aria-label="Rules">
            <Pill>Incoming request</Pill>
            <Connector />
            {drafts.map((d, i) => (
              <div key={d.id} role="listitem">
                <RuleCard
                  d={d}
                  index={i}
                  errors={check?.rules[i] ?? []}
                  collapsed={collapsed.has(d.id)}
                  dragging={drag?.from === i}
                  dropTarget={drag !== null && drag.over === i && drag.from !== i}
                  onToggle={() =>
                    setCollapsed((s) => {
                      const n = new Set(s);
                      if (!n.delete(d.id)) n.add(d.id);
                      return n;
                    })
                  }
                  onChange={(r) => setRule(d.id, r)}
                  onRemove={() => setDrafts((ds) => ds.filter((x) => x.id !== d.id))}
                  onMove={(delta) => move(i, i + delta, d.id)}
                  onDragStart={() => setDrag({ from: i, over: null })}
                  onDragEnd={() => setDrag(null)}
                  onDragOver={() => setDrag((s) => (s && s.over !== i ? { ...s, over: i } : s))}
                  onDrop={() => {
                    if (drag) move(drag.from, i);
                    setDrag(null);
                  }}
                />
                <Connector />
              </div>
            ))}
            <div className="mx-auto">
              <Button variant="outline" icon={<Plus />} disabled={atLimit} title={atLimit ? `At most ${MAX_EDGE_RULES} rules` : undefined} onClick={addRule}>
                Add rule
              </Button>
            </div>
            <Connector />
            <Pill>No match – request continues to your service</Pill>
          </div>
        )}

        {check?.general ? <Alert>{check.general}</Alert> : null}
        {error ? <Alert>{error}</Alert> : null}

        <div className="flex items-center justify-between gap-3 border-t border-border pt-4">
          {confirmDiscard ? (
            <>
              <span className="text-sm">Discard unsaved changes?</span>
              <div className="flex gap-2">
                <Button variant="secondary" onClick={() => setConfirmDiscard(false)}>
                  Keep editing
                </Button>
                <Button variant="danger" onClick={() => onOpenChange(false)}>
                  Discard
                </Button>
              </div>
            </>
          ) : (
            <>
              <div className="flex items-center gap-3 text-sm text-muted-foreground">
                <span className="tabular-nums">
                  {count} of {MAX_EDGE_RULES} rules
                </span>
                <Button size="sm" variant="ghost" disabled={jsonMode && !parsed?.ok} onClick={toggleJson}>
                  {jsonMode ? "Back to editor" : "Edit as JSON"}
                </Button>
              </div>
              <div className="flex gap-2">
                <Button variant="secondary" onClick={requestClose}>
                  Cancel
                </Button>
                <Button variant="primary" loading={saving} disabled={!canSave} onClick={() => void save()}>
                  Save
                </Button>
              </div>
            </>
          )}
        </div>
      </div>
    </Modal>
  );
}
