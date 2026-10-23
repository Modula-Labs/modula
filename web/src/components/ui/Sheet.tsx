import { clsx } from "clsx";
import { useEffect, type ReactNode } from "react";
import { X } from "./icons";

/** A right-side sheet inside the project area. `stacked` offsets a second
 *  sheet so the one below still peeks out. Esc closes the top-most sheet. */
export function Sheet({
  onClose,
  header,
  children,
  stacked,
}: {
  onClose: () => void;
  header: ReactNode;
  children: ReactNode;
  stacked?: boolean;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) {
        e.preventDefault();
        onClose();
      }
    };
    // Capture so the stacked (later-mounted) sheet handles Esc first.
    window.addEventListener("keydown", onKey, { capture: stacked });
    return () => window.removeEventListener("keydown", onKey, { capture: stacked });
  }, [onClose, stacked]);
  return (
    <section
      className={clsx(
        "anim-sheet absolute top-0 right-0 bottom-0 flex flex-col rounded-tl-xl border-t border-l border-border bg-card shadow-modal",
        stacked ? "z-30 w-[min(960px,calc(60%-24px))] min-w-[616px]" : "z-20 w-[min(960px,60%)] min-w-[640px]",
      )}
    >
      <header className="flex items-start justify-between gap-4 px-6 pt-5 pb-4">
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
