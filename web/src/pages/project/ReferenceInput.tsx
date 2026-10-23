import { clsx } from "clsx";
import { useEffect, useRef, useState, type ComponentProps } from "react";
import { Input } from "@/components/ui/Input";
import { filterReferenceOptions, findReferenceQuery, insertReference, type ReferenceOption } from "@/lib/references";

/** A value input that completes `${{…}}` references: typing `${{` lists
 *  shared variables, other services' variables and platform variables. */
export function ReferenceInput({
  value,
  onValueChange,
  options,
  ...rest
}: Omit<ComponentProps<typeof Input>, "value" | "onChange"> & {
  value: string;
  onValueChange: (v: string) => void;
  options: ReferenceOption[];
}) {
  const input = useRef<HTMLInputElement>(null);
  const activeItem = useRef<HTMLButtonElement>(null);
  const [caret, setCaret] = useState(0);
  const [open, setOpen] = useState(true);
  const [active, setActive] = useState(0);

  const q = findReferenceQuery(value, caret);
  const items = q ? filterReferenceOptions(options, q.query) : [];
  const show = !!q && open && items.length > 0;
  useEffect(() => activeItem.current?.scrollIntoView({ block: "nearest" }), [active, show]);

  const pick = (o: ReferenceOption) => {
    if (!q) return;
    const next = insertReference(value, caret, q, o.path);
    onValueChange(next.value);
    setCaret(next.caret);
    requestAnimationFrame(() => {
      input.current?.focus();
      input.current?.setSelectionRange(next.caret, next.caret);
    });
  };

  return (
    <div className="relative min-w-0 flex-1">
      <Input
        {...rest}
        ref={input}
        value={value}
        autoComplete="off"
        onChange={(e) => {
          onValueChange(e.target.value);
          setCaret(e.target.selectionStart ?? e.target.value.length);
          setOpen(true);
          setActive(0);
        }}
        onSelect={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
        onBlur={() => setOpen(false)}
        onFocus={() => setOpen(true)}
        onKeyDown={(e) => {
          if (!show) return;
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            setActive((a) => (a + (e.key === "ArrowDown" ? 1 : -1) + items.length) % items.length);
          } else if (e.key === "Enter" || e.key === "Tab") {
            e.preventDefault();
            pick(items[Math.min(active, items.length - 1)]!);
          } else if (e.key === "Escape") {
            // Handled here so Esc doesn't also close the sheet.
            e.preventDefault();
            setOpen(false);
          }
        }}
      />
      {show ? (
        <div
          role="listbox"
          aria-label="Variable references"
          className="anim-pop absolute top-full left-0 z-50 mt-1 max-h-60 w-full min-w-64 overflow-auto rounded-lg bg-popover p-1 text-popover-foreground shadow-menu"
        >
          {items.map((o, i) => (
            <div key={o.label}>
              {i === 0 || items[i - 1]!.group !== o.group ? (
                <div className="text-caps px-2 pt-1.5 pb-1 text-muted-foreground">{o.group}</div>
              ) : null}
              <button
                type="button"
                role="option"
                aria-selected={i === active}
                ref={i === active ? activeItem : undefined}
                // Keep focus in the input so the caret and blur handling stay put.
                onMouseDown={(e) => {
                  e.preventDefault();
                  pick(o);
                }}
                onMouseEnter={() => setActive(i)}
                className={clsx(
                  "flex h-8 w-full items-center justify-between gap-2 rounded-md px-2 text-left font-mono text-[13px]",
                  i === active && "bg-accent",
                )}
              >
                <span className="truncate">{o.label}</span>
                {o.platform ? <span className="text-xs text-muted-foreground">platform</span> : null}
              </button>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}
