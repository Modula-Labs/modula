import { clsx } from "clsx";
import { useEffect, useState, type ReactNode } from "react";
import { X } from "./icons";

/** Keeps `value` around for `ms` after it goes undefined so an exit animation
 *  can play; `leaving` is true during that window. No delay under reduced motion. */
export function useLingering<T>(value: T | undefined, ms = 200): { value: T | undefined; leaving: boolean } {
  const [kept, setKept] = useState(value);
  useEffect(() => {
    if (value !== undefined) {
      setKept(value);
      return;
    }
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    if (reduced) {
      setKept(undefined);
      return;
    }
    const t = window.setTimeout(() => setKept(undefined), ms);
    return () => window.clearTimeout(t);
  }, [value, ms]);
  return value !== undefined ? { value, leaving: false } : { value: kept ?? undefined, leaving: kept !== undefined };
}

/** A right-side sheet inside the project area. `stacked` offsets a second
 *  sheet so the one below still peeks out. Esc closes the top-most sheet. */
export function Sheet({
  onClose,
  header,
  children,
  stacked,
  leaving,
}: {
  onClose: () => void;
  header: ReactNode;
  children: ReactNode;
  stacked?: boolean;
  /** Plays the exit animation; the parent unmounts it afterwards (see `useLingering`). */
  leaving?: boolean;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented && !leaving) {
        e.preventDefault();
        onClose();
      }
    };
    // Capture so the stacked (later-mounted) sheet handles Esc first.
    window.addEventListener("keydown", onKey, { capture: stacked });
    return () => window.removeEventListener("keydown", onKey, { capture: stacked });
  }, [onClose, stacked, leaving]);
  return (
    <section
      className={clsx(
        leaving ? "anim-sheet-out" : "anim-sheet",
        // Floats inside the canvas, clear of the navbar and edges (like Railway's panel).
        "absolute top-3 right-3 bottom-3 flex flex-col overflow-hidden rounded-xl border border-border bg-card shadow-modal",
        // Wide enough to work in, never flush with the canvas's left edge.
        stacked
          ? "z-30 w-[min(960px,max(616px,calc(60%_-_24px)),calc(100%_-_48px))]"
          : "z-20 w-[min(960px,max(640px,60%),calc(100%_-_24px))]",
      )}
    >
      <header className="flex items-start justify-between gap-4 px-6 pt-6 pb-4 md:px-10">
        <div className="min-w-0 flex-1">{header}</div>
        <button
          onClick={onClose}
          aria-label="Close"
          className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          <X className="size-4" />
        </button>
      </header>
      {children}
    </section>
  );
}
