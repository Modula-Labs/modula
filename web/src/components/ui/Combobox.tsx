import { clsx } from "clsx";
import { useId, useState, type ReactNode } from "react";
import { moveHighlight } from "@/lib/github";
import { Input } from "./Input";

/**
 * A text input with suggestions (ARIA 1.2 combobox). Typing stays free text;
 * ↑/↓ move through the suggestions, Enter picks the highlighted one, Esc closes
 * the list. The list renders in the flow, under the input, so it is never
 * clipped by a dialog.
 */
export function Combobox<T>({
  inputProps,
  value,
  onChange,
  options,
  optionKey,
  renderOption,
  onPick,
  loading,
  empty,
  placeholder,
  autoFocus,
  disabled,
  mono,
}: {
  /** From `Field` (id, aria-invalid, aria-describedby). */
  inputProps: { id: string; "aria-invalid": boolean; "aria-describedby"?: string };
  value: string;
  onChange: (v: string) => void;
  options: T[];
  optionKey: (t: T) => string;
  renderOption: (t: T) => ReactNode;
  onPick: (t: T) => void;
  loading?: boolean;
  /** Shown when the list is open but has nothing to suggest. */
  empty?: ReactNode;
  placeholder?: string;
  autoFocus?: boolean;
  disabled?: boolean;
  mono?: boolean;
}) {
  const listId = useId();
  const [open, setOpen] = useState(false);
  const [highlight, setHighlight] = useState(-1);
  const shown = open && (options.length > 0 || loading || !!empty);
  const optionId = (i: number) => `${listId}-${i}`;

  const pick = (t: T) => {
    onPick(t);
    setOpen(false);
    setHighlight(-1);
  };

  return (
    <div className="flex flex-col gap-1">
      <Input
        {...inputProps}
        mono={mono}
        autoFocus={autoFocus}
        disabled={disabled}
        autoComplete="off"
        spellCheck={false}
        placeholder={placeholder}
        role="combobox"
        aria-expanded={shown}
        aria-controls={listId}
        aria-autocomplete="list"
        aria-activedescendant={shown && highlight >= 0 ? optionId(highlight) : undefined}
        value={value}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
          setHighlight(-1);
        }}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            setOpen(true);
            setHighlight((h) => moveHighlight(h, options.length, e.key === "ArrowDown" ? 1 : -1));
          } else if (e.key === "Enter" && shown && highlight >= 0 && options[highlight]) {
            e.preventDefault();
            pick(options[highlight]);
          } else if (e.key === "Escape" && shown) {
            // The dialog skips its own Escape while a list is open (see NewServiceDialog).
            e.preventDefault();
            setOpen(false);
            setHighlight(-1);
          }
        }}
      />
      {shown ? (
        <ul
          id={listId}
          role="listbox"
          className="max-h-60 overflow-y-auto rounded-md border border-border bg-card p-1 shadow-raise"
        >
          {options.map((t, i) => (
            <li
              key={optionKey(t)}
              id={optionId(i)}
              role="option"
              aria-selected={i === highlight}
              // Keep focus in the input so the list doesn't close before the click.
              onMouseDown={(e) => e.preventDefault()}
              onMouseEnter={() => setHighlight(i)}
              onClick={() => pick(t)}
              className={clsx(
                "flex h-9 cursor-pointer items-center gap-2 rounded-sm px-2 text-sm",
                i === highlight && "bg-accent",
              )}
            >
              {renderOption(t)}
            </li>
          ))}
          {options.length === 0 ? (
            <li className="px-2 py-2 text-sm text-muted-foreground">{loading ? "Loading…" : empty}</li>
          ) : null}
        </ul>
      ) : null}
    </div>
  );
}
