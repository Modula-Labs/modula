import { IconAlertCircle, IconAlertTriangle, IconCircleCheck, IconInfoCircle } from "@tabler/icons-react";
import { clsx } from "clsx";
import type { ComponentType, ReactNode } from "react";

/** Kobra keycap: 16/20/24/28px tall, 1px border on the muted fill, 10–14px text.
 *  `bare` drops the border and sits on a 10% wash (inside tooltips and menus). */
const kbdSizes = {
  sm: "h-4 min-w-4 rounded-sm px-1 text-[10px] leading-4",
  md: "h-5 min-w-5 rounded-sm px-1 text-xs leading-4",
  lg: "h-6 min-w-6 rounded-md px-1.5 text-[13px] leading-5",
  xl: "h-7 min-w-7 rounded-lg px-2 text-sm leading-5",
} as const;

export function Kbd({ children, size = "md", bare }: { children: ReactNode; size?: keyof typeof kbdSizes; bare?: boolean }) {
  return (
    <kbd
      className={clsx(
        "inline-flex shrink-0 items-center justify-center gap-1 font-sans font-normal text-foreground select-none",
        kbdSizes[size],
        bare ? "bg-foreground/10" : "border border-border bg-muted",
      )}
    >
      {children}
    </kbd>
  );
}

/** Kobra pill badges: 24px, 8px sides, 14/20 regular, sentence case. Colour carries
 *  status only; emphasis stays neutral. */
export type Tone = "neutral" | "success" | "warn" | "danger" | "info" | "strong";
const tones: Record<Tone, string> = {
  neutral: "bg-foreground/10 text-muted-foreground",
  success: "bg-badge-green-bg text-badge-green-fg",
  warn: "bg-badge-amber-bg text-badge-amber-fg",
  danger: "bg-badge-red-bg text-badge-red-fg",
  info: "bg-badge-blue-bg text-badge-blue-fg",
  strong: "bg-badge-violet-bg text-badge-violet-fg",
};

export function Badge({ tone = "neutral", icon, children }: { tone?: Tone; icon?: ReactNode; children: ReactNode }) {
  return (
    <span
      className={clsx(
        "inline-flex h-6 shrink-0 items-center gap-1 rounded-full px-2 text-sm leading-5 font-normal whitespace-nowrap [&_svg]:size-3",
        tones[tone],
      )}
    >
      {icon}
      {children}
    </span>
  );
}

/** Kobra Alert: radius lg, the tone at 10% under tone-coloured text, a 32px tile in
 *  the tone with a 20px mark, bold title over a regular description. */
const alertTones: Record<Tone, { wash: string; tile: string; Icon: ComponentType<{ className?: string; stroke?: number }> }> = {
  danger: { wash: "bg-error/10 text-error", tile: "bg-error", Icon: IconAlertCircle },
  warn: { wash: "bg-warning/10 text-warning", tile: "bg-warning", Icon: IconAlertTriangle },
  success: { wash: "bg-success/10 text-success", tile: "bg-success", Icon: IconCircleCheck },
  info: { wash: "bg-info/10 text-info", tile: "bg-info", Icon: IconInfoCircle },
  // Not Kobra tones: kept for existing callers, drawn neutral.
  neutral: { wash: "bg-muted text-foreground", tile: "bg-muted-foreground", Icon: IconInfoCircle },
  strong: { wash: "bg-muted text-foreground", tile: "bg-foreground", Icon: IconInfoCircle },
};

export function Alert({ tone = "danger", title, children }: { tone?: Tone; title?: string; children?: ReactNode }) {
  const t = alertTones[tone];
  return (
    <div role="alert" className={clsx("flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm", t.wash)}>
      <span
        aria-hidden
        // White mark on the tone in light; in dark the tones are light, so the mark takes the page colour.
        className={clsx("flex size-8 shrink-0 items-center justify-center self-start rounded-md text-white dark:text-background", t.tile)}
      >
        <t.Icon className="size-5" stroke={1.8} />
      </span>
      <div className="flex min-h-8 min-w-0 flex-1 flex-col justify-center">
        {title ? <div className="font-bold">{title}</div> : null}
        {children ? <div className="font-normal">{children}</div> : null}
      </div>
    </div>
  );
}

/** Kobra Empty: up to 448px, a radius xl dashed region with 24px inset; a 40px muted
 *  tile with a 24px icon over a bold title and relaxed muted description. */
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
    <div className="mx-auto flex w-full max-w-md flex-col items-center gap-6 rounded-xl border border-dashed border-border p-6 text-center">
      <div className="flex flex-col items-center gap-1">
        <div className="mb-4 flex size-10 items-center justify-center rounded-lg bg-muted text-foreground [&>svg]:size-6">{icon}</div>
        <div className="text-base leading-6 font-bold">{title}</div>
        <div className="text-sm leading-[22px] text-muted-foreground">{body}</div>
      </div>
      {action ? <div className="flex flex-wrap items-center justify-center gap-2.5">{action}</div> : null}
    </div>
  );
}

/** Kobra Skeleton: radius md; neutral-200 in light (the border grey), muted in dark. */
export function Skeleton({ className }: { className?: string }) {
  return <div className={clsx("animate-pulse rounded-md bg-border dark:bg-muted", className)} />;
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
