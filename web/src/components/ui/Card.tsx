import { clsx } from "clsx";
import { createContext, useContext, type ReactNode } from "react";

/* Kobra Card: radius xl surface with a 1px foreground ring at 10%. Default spaces its
   parts 16px apart and insets them 16px; SM uses 12px. The footer sits on a muted
   wash behind a top border and right-aligns its buttons. Title is regular 16/22
   (SM 14/20), description muted 14/20, the action sits top right. */

type Size = "default" | "sm";
const SizeContext = createContext<Size>("default");

export function Card({ size = "default", className, children }: { size?: Size; className?: string; children: ReactNode }) {
  return (
    <SizeContext.Provider value={size}>
      <div
        className={clsx(
          "flex flex-col overflow-hidden rounded-xl bg-card text-card-foreground ring-1 ring-foreground/10",
          // Bottom inset comes from the last part: content pads itself, the footer has its own.
          size === "sm" ? "gap-3 pt-3 [&:not(:has(>[data-card-footer]))]:pb-3" : "gap-4 pt-4 [&:not(:has(>[data-card-footer]))]:pb-4",
          className,
        )}
      >
        {children}
      </div>
    </SizeContext.Provider>
  );
}

export function CardHeader({
  title,
  description,
  action,
  className,
}: {
  title: ReactNode;
  description?: ReactNode;
  action?: ReactNode;
  className?: string;
}) {
  const sm = useContext(SizeContext) === "sm";
  return (
    <div className={clsx("flex items-start gap-4", sm ? "px-3" : "px-4", className)}>
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <div className={clsx("font-normal text-foreground", sm ? "text-sm leading-5" : "text-base leading-[22px]")}>{title}</div>
        {description ? <div className="text-sm text-muted-foreground">{description}</div> : null}
      </div>
      {action ? <div className="shrink-0">{action}</div> : null}
    </div>
  );
}

export function CardContent({ className, children }: { className?: string; children: ReactNode }) {
  const sm = useContext(SizeContext) === "sm";
  return <div className={clsx(sm ? "px-3" : "px-4", className)}>{children}</div>;
}

export function CardFooter({ className, children }: { className?: string; children: ReactNode }) {
  const sm = useContext(SizeContext) === "sm";
  return (
    <div
      data-card-footer=""
      className={clsx("flex items-center justify-end gap-2 border-t border-border bg-muted/50", sm ? "p-3" : "p-4", className)}
    >
      {children}
    </div>
  );
}
