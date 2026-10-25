import { clsx } from "clsx";
import { forwardRef, useEffect, type ReactNode, type RefObject } from "react";
import { Button } from "@/components/ui/Button";
import { ArrowDown, ArrowUp, Search } from "@/components/ui/icons";
import { Kbd } from "@/components/ui/misc";

/** A log search box. `slashFocus`: `/` anywhere outside a field focuses it. */
export const LogSearch = forwardRef<
  HTMLInputElement,
  { value: string; onChange: (v: string) => void; placeholder: string; slashFocus?: boolean; invalid?: boolean }
>(function LogSearch({ value, onChange, placeholder, slashFocus, invalid }, ref) {
  useEffect(() => {
    if (!slashFocus) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "/" || e.metaKey || e.ctrlKey || e.altKey) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return;
      const input = (ref as RefObject<HTMLInputElement | null> | null)?.current;
      // Only the visible box (a hidden tab's viewer stays mounted).
      if (!input || input.offsetParent === null) return;
      e.preventDefault();
      input.focus();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [slashFocus, ref]);
  return (
    <label
      className={clsx(
        "flex h-8 min-w-0 flex-1 items-center gap-2 rounded-lg border px-2.5 text-muted-foreground focus-within:ring-3 dark:bg-input/30",
        invalid
          ? "border-destructive focus-within:ring-destructive/20"
          : "border-input focus-within:border-ring focus-within:ring-ring/50",
      )}
    >
      <Search className="size-4 shrink-0" />
      <input
        ref={ref}
        className="min-w-0 flex-1 bg-transparent text-sm text-foreground outline-none placeholder:text-muted-foreground"
        placeholder={placeholder}
        aria-label={placeholder}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
      {slashFocus && !value ? <Kbd size="sm">/</Kbd> : null}
    </label>
  );
});

/** Floating jump-to-top/bottom buttons for a scrolled log list. */
export function JumpButtons({ box, children }: { box: RefObject<HTMLDivElement | null>; children?: ReactNode }) {
  const go = (top: boolean) => {
    const el = box.current;
    if (el) el.scrollTo({ top: top ? 0 : el.scrollHeight });
  };
  return (
    <div className="absolute right-3 bottom-3 flex flex-col gap-1">
      {children}
      <Button variant="outline" size="icon-sm" aria-label="Jump to top" title="Jump to top" className="shadow-sm" onClick={() => go(true)}>
        <ArrowUp />
      </Button>
      <Button variant="outline" size="icon-sm" aria-label="Jump to bottom" title="Jump to bottom" className="shadow-sm" onClick={() => go(false)}>
        <ArrowDown />
      </Button>
    </div>
  );
}

/** Whether a scrolled box is (about) at its bottom, to keep following new rows. */
export const atBottom = (el: HTMLElement) => el.scrollHeight - el.scrollTop - el.clientHeight < 24;

/** Scrolls a row of the list into view and flashes it ("View in context"). */
export function revealRow(box: HTMLElement | null, key: string) {
  const row = box?.querySelector<HTMLElement>(`[data-row="${CSS.escape(key)}"]`);
  if (!row) return;
  row.scrollIntoView({ block: "center" });
  row.classList.add("bg-accent");
  setTimeout(() => row.classList.remove("bg-accent"), 1600);
}
