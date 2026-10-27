import { clsx } from "clsx";
import { Button } from "@/components/ui/Button";
import { AlertCircle, ExternalLink, X } from "@/components/ui/icons";
import type { EnvWarning } from "@/lib/api";

const TONE: Record<EnvWarning["severity"], string> = {
  error: "text-destructive",
  warning: "text-badge-amber-fg",
  info: "text-muted-foreground",
};

/** Floating list over the canvas (where Activity sits): what needs attention in this environment, worst first. */
export function WarningsPanel({
  items,
  onOpen,
  onClose,
}: {
  items: EnvWarning[];
  onOpen: (w: EnvWarning) => void;
  onClose: () => void;
}) {
  return (
    <aside
      aria-label="Warnings"
      className="anim-sheet absolute top-14 right-3 bottom-3 z-10 flex w-[380px] max-w-[calc(100%-24px)] flex-col rounded-xl border border-border bg-popover shadow-menu"
    >
      <header className="flex items-center justify-between px-4 pt-3 pb-2">
        <h2 className="text-base font-bold">Warnings</h2>
        <Button variant="ghost" size="icon-sm" onClick={onClose} aria-label="Close warnings">
          <X />
        </Button>
      </header>
      <ul className="min-h-0 flex-1 overflow-y-auto border-t border-border">
        {items.length === 0 ? <li className="p-4 text-sm text-muted-foreground">Nothing needs attention.</li> : null}
        {items.map((w, i) => (
          <li key={`${w.kind}-${w.serviceInstanceId ?? ""}-${i}`} className="border-b border-border last:border-b-0">
            <div className="flex gap-3 px-4 py-3">
              <AlertCircle className={clsx("mt-0.5 size-4 shrink-0", TONE[w.severity])} aria-label={w.severity} />
              <div className="flex min-w-0 flex-1 flex-col gap-1">
                <button onClick={() => onOpen(w)} className="cursor-pointer text-left text-sm font-bold hover:underline">
                  {w.title}
                </button>
                <p className="text-sm text-muted-foreground">{w.detail}</p>
                <a
                  href={w.docsUrl}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex w-fit items-center gap-1 text-xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
                >
                  Why? <ExternalLink className="size-3" />
                </a>
              </div>
            </div>
          </li>
        ))}
      </ul>
    </aside>
  );
}
