import { clsx } from "clsx";
import type { ReactNode } from "react";

export function Kbd({ children }: { children: ReactNode }) {
  return (
    <kbd className="inline-flex h-[18px] min-w-[18px] items-center justify-center rounded-xs border border-border bg-surface-3 px-1 font-mono text-[11px] text-fg-muted">
      {children}
    </kbd>
  );
}

type Tone = "neutral" | "success" | "warn" | "danger" | "info" | "accent";
const tones: Record<Tone, string> = {
  neutral: "bg-surface-3 text-fg-muted",
  success: "bg-success-subtle text-success",
  warn: "bg-warn-subtle text-warn",
  danger: "bg-danger-subtle text-danger",
  info: "bg-info-subtle text-info",
  accent: "bg-accent-subtle text-accent-text",
};

export function Badge({ tone = "neutral", children }: { tone?: Tone; children: ReactNode }) {
  return <span className={clsx("text-caps inline-flex h-5 items-center rounded-xs px-1.5", tones[tone])}>{children}</span>;
}

export function Alert({ tone = "danger", title, children }: { tone?: Tone; title?: string; children?: ReactNode }) {
  return (
    <div
      role="alert"
      className={clsx(
        "rounded-md border px-3 py-2.5 text-sm",
        tone === "danger" && "border-danger/40 bg-danger-subtle text-danger",
        tone === "warn" && "border-warn/40 bg-warn-subtle text-warn",
        tone === "info" && "border-info/40 bg-info-subtle text-info",
        tone === "success" && "border-success/40 bg-success-subtle text-success",
        (tone === "neutral" || tone === "accent") && "border-accent-border bg-accent-subtle text-accent-text",
      )}
    >
      {title ? <div className="font-semibold">{title}</div> : null}
      {children}
    </div>
  );
}

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
    <div className="flex flex-col items-center gap-2 rounded-lg border border-dashed border-border px-6 py-12 text-center">
      <div className="text-fg-subtle [&>svg]:size-6">{icon}</div>
      <div className="text-base font-semibold">{title}</div>
      <div className="max-w-sm text-sm text-fg-muted">{body}</div>
      {action ? <div className="mt-3">{action}</div> : null}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={clsx("animate-pulse rounded-md bg-surface-3", className)} />;
}
