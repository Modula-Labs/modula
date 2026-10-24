import clsx from "clsx";
import { Link } from "@tanstack/react-router";
import { useRef, type KeyboardEvent } from "react";
import { Segmented } from "@/components/ui/Tabs";
import { codeBytes, FUNCTION_TEMPLATES, indent, newline, type Edit, type FunctionTarget } from "@/lib/functions";

/**
 * A plain code editor: a monospace textarea where Tab indents (Shift+Tab
 * outdents) and Enter keeps the indentation. Esc then Tab leaves it, so the
 * keyboard is never trapped.
 */
export function CodeEditor({
  id,
  value,
  onChange,
  disabled,
  rows = 18,
  ...aria
}: {
  id?: string;
  value: string;
  onChange: (v: string) => void;
  disabled?: boolean;
  rows?: number;
  "aria-invalid"?: boolean;
  "aria-describedby"?: string;
  "aria-label"?: string;
}) {
  const ref = useRef<HTMLTextAreaElement>(null);
  // After Esc, the next Tab moves focus as usual.
  const escaped = useRef(false);
  const apply = (next: Edit) => {
    onChange(next.value);
    requestAnimationFrame(() => ref.current?.setSelectionRange(next.selectionStart, next.selectionEnd));
  };
  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    const t = e.currentTarget;
    const at: Edit = { value: t.value, selectionStart: t.selectionStart, selectionEnd: t.selectionEnd };
    if (e.key === "Escape") {
      escaped.current = true;
      return;
    }
    if (e.key === "Tab" && !escaped.current && !e.altKey && !e.ctrlKey && !e.metaKey) {
      e.preventDefault();
      apply(indent(at, e.shiftKey));
      return;
    }
    escaped.current = false;
    if (e.key === "Enter" && !e.shiftKey && !e.altKey && !e.ctrlKey && !e.metaKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      apply(newline(at));
    }
  };
  return (
    <textarea
      ref={ref}
      id={id}
      {...aria}
      value={value}
      disabled={disabled}
      rows={rows}
      spellCheck={false}
      autoCapitalize="off"
      autoComplete="off"
      autoCorrect="off"
      wrap="off"
      onChange={(e) => onChange(e.target.value)}
      onKeyDown={onKeyDown}
      className={clsx(
        "w-full resize-y rounded-lg border border-input bg-transparent px-3 py-2.5 font-mono text-[13px] leading-5 [tab-size:2]",
        "outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:opacity-50",
        "aria-invalid:border-destructive",
      )}
    />
  );
}

/** The code's size, against the 1 MB limit. */
export function CodeSize({ code }: { code: string }) {
  const kb = codeBytes(code) / 1024;
  return <span className="text-xs text-muted-foreground tabular-nums">{kb < 10 ? kb.toFixed(1) : Math.round(kb)} KB of 1024 KB</span>;
}

/** Starting points for a new function. */
export function TemplatePicker({ value, onPick }: { value: string; onPick: (id: string, code: string) => void }) {
  return (
    <div role="radiogroup" aria-label="Template" className="grid grid-cols-3 gap-2">
      {FUNCTION_TEMPLATES.map((t) => (
        <button
          key={t.id}
          type="button"
          role="radio"
          aria-checked={t.id === value}
          onClick={() => onPick(t.id, t.code)}
          className={clsx(
            "flex cursor-pointer flex-col gap-0.5 rounded-xl border p-2.5 text-left hover:bg-accent",
            t.id === value ? "border-foreground" : "border-border",
          )}
        >
          <span className="text-sm font-medium">{t.name}</span>
          <span className="line-clamp-2 text-xs text-muted-foreground">{t.description}</span>
        </button>
      ))}
    </div>
  );
}

/** Where the function runs: your servers (workerd) or Cloudflare Workers. */
export function TargetField({
  target,
  onTarget,
  connected,
  canEdit = true,
}: {
  target: FunctionTarget;
  onTarget: (t: FunctionTarget) => void;
  /** Whether the workspace has Cloudflare connected (undefined: still loading). */
  connected: boolean | undefined;
  canEdit?: boolean;
}) {
  // A function already on Cloudflare keeps its choice visible even if the connection is gone.
  const canChoose = canEdit && (connected === true || target === "cloudflare");
  return (
    <div className="flex flex-col gap-1.5">
      <div className="text-sm leading-none font-normal">Runs on</div>
      <div className={canChoose ? undefined : "pointer-events-none opacity-50"}>
        <Segmented
          label="Runs on"
          value={target}
          onChange={onTarget}
          options={[
            { value: "self", label: "Your servers" },
            { value: "cloudflare", label: "Cloudflare" },
          ]}
        />
      </div>
      <p className="text-xs text-muted-foreground">
        {target === "cloudflare"
          ? "Uploaded to Cloudflare Workers, with its variables as bindings. Logs are in the Cloudflare dashboard."
          : "Runs in workerd on your servers, always warm. Its console output is the deployment's logs."}
        {connected === false && target === "self" ? (
          <>
            {" "}
            Cloudflare isn't connected.{" "}
            <Link to="/settings" className="underline">
              Connect it in Settings → Integrations
            </Link>
            .
          </>
        ) : null}
      </p>
    </div>
  );
}
