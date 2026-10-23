import { clsx } from "clsx";
import type { ReactNode } from "react";

export function Kbd({ children }: { children: ReactNode }) {
  return (
    <kbd className="inline-flex h-[18px] min-w-[18px] items-center justify-center rounded-xs border border-border bg-muted px-1 font-mono text-[11px] text-muted-foreground">
      {children}
    </kbd>
  );
}

/** Kobra pill badges. Colour carries status only; emphasis stays neutral. */
export type Tone = "neutral" | "success" | "warn" | "danger" | "info" | "strong";
const tones: Record<Tone, string> = {
  neutral: "bg-badge-neutral-bg text-badge-neutral-fg",
  success: "bg-badge-green-bg text-badge-green-fg",
  warn: "bg-badge-amber-bg text-badge-amber-fg",
  danger: "bg-badge-red-bg text-badge-red-fg",
  info: "bg-badge-blue-bg text-badge-blue-fg",
  strong: "bg-badge-violet-bg text-badge-violet-fg",
};

export function Badge({ tone = "neutral", children }: { tone?: Tone; children: ReactNode }) {
  return (
    <span className={clsx("text-caps inline-flex h-5 items-center rounded-full px-2", tones[tone])}>{children}</span>
  );
}

export function Alert({ tone = "danger", title, children }: { tone?: Tone; title?: string; children?: ReactNode }) {
  return (
    <div role="alert" className={clsx("rounded-lg border px-3 py-2.5 text-sm", alertTones[tone])}>
      {title ? <div className="font-semibold">{title}</div> : null}
      {children}
    </div>
  );
}

const alertTones: Record<Tone, string> = {
  danger: "border-destructive/40 bg-badge-red-bg text-badge-red-fg",
  warn: "border-warning/40 bg-badge-amber-bg text-badge-amber-fg",
  info: "border-info/40 bg-badge-blue-bg text-badge-blue-fg",
  success: "border-success/40 bg-badge-green-bg text-badge-green-fg",
  neutral: "border-border bg-muted text-foreground",
  strong: "border-border bg-muted text-foreground",
};

export function EmptyState({
  icon,
  title,
  body,
  action,
}: {
  icon: ReactNode;
  title: string;
  body: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center gap-2 rounded-xl border border-dashed border-border px-6 py-12 text-center">
      <div className="text-muted-foreground [&>svg]:size-6">{icon}</div>
      <div className="text-base font-semibold">{title}</div>
      <div className="max-w-sm text-sm text-muted-foreground">{body}</div>
      {action ? <div className="mt-3">{action}</div> : null}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={clsx("animate-pulse rounded-md bg-muted", className)} />;
}

/** A small status dot + label, e.g. ● Online. */
export function StatusDot({ tone, children }: { tone: "success" | "warn" | "danger" | "neutral"; children: ReactNode }) {
  return (
    <span className="inline-flex items-center gap-1.5 text-sm">
      <span
        aria-hidden
        className={clsx(
          "size-2 rounded-full",
          tone === "success" && "bg-success",
          tone === "warn" && "bg-warning",
          tone === "danger" && "bg-error",
          tone === "neutral" && "bg-muted-foreground",
        )}
      />
      <span className={clsx(tone === "neutral" ? "text-muted-foreground" : "text-foreground")}>{children}</span>
    </span>
  );
}
