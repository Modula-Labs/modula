import { clsx } from "clsx";
import { useEffect, useState, type ReactNode } from "react";
import { ErrorBoundary } from "../ErrorBoundary";
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
        // Kobra Sheet: inset 12px from the canvas, radius 2xl, shadow-xl + 1px foreground ring.
        "absolute top-3 right-3 bottom-3 flex flex-col overflow-hidden rounded-2xl bg-card text-card-foreground shadow-xl ring-1 ring-foreground/10",
        // Wide enough to work in, never flush with the canvas's left edge.
        stacked
          ? "z-30 w-[min(960px,max(616px,calc(60%_-_24px)),calc(100%_-_48px))]"
          : "z-20 w-[min(960px,max(640px,60%),calc(100%_-_24px))]",
      )}
    >
      <header className="relative px-6 pt-6 pr-16 pb-4 md:px-10 md:pr-16">
        <div className="min-w-0">{header}</div>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close"
          className="absolute top-4 right-4 flex size-8 items-center justify-center rounded-full bg-foreground/5 text-muted-foreground transition-colors hover:bg-foreground/10 hover:text-foreground"
        >
          <X className="size-4" />
        </button>
      </header>
      <ErrorBoundary>{children}</ErrorBoundary>
    </section>
  );
}
