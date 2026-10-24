import { clsx } from "clsx";
import type { ButtonHTMLAttributes, ReactNode } from "react";

/**
 * Kobra button. `primary` is the one teal action per screen (with our raise);
 * everything else is neutral. `md` is Kobra's 32px default.
 */
type Variant = "primary" | "secondary" | "outline" | "ghost" | "danger" | "danger-outline" | "link";
type Size = "xs" | "sm" | "md" | "lg" | "icon" | "icon-sm" | "icon-xs";

const variants: Record<Variant, string> = {
  primary: "border-transparent bg-primary text-primary-foreground shadow-raise hover:bg-primary/85",
  secondary: "border-transparent bg-secondary text-secondary-foreground hover:bg-input",
  outline:
    "border-border bg-background text-foreground hover:bg-accent dark:border-input dark:bg-input/30 dark:hover:bg-input/50",
  ghost: "border-transparent text-foreground hover:bg-foreground/7",
  danger:
    "border-transparent bg-destructive text-white hover:bg-destructive/90 focus-visible:ring-destructive/20 dark:bg-destructive/60 dark:hover:bg-destructive/70 dark:focus-visible:ring-destructive/40",
  "danger-outline":
    "border-border bg-background text-error hover:bg-destructive/10 focus-visible:ring-destructive/20 dark:border-input dark:bg-input/30 dark:hover:bg-destructive/15",
  link: "border-transparent text-foreground underline-offset-4 hover:underline",
};

const sizes: Record<Size, string> = {
  xs: "h-6 gap-1 rounded-md px-2 text-xs [&_svg]:size-3",
  sm: "h-7 gap-1 rounded-md px-2.5 text-[13px] [&_svg]:size-3.5",
  md: "h-8 gap-1.5 rounded-lg px-2.5 text-sm [&_svg]:size-4",
  lg: "h-9 gap-2 rounded-lg px-4 text-sm [&_svg]:size-4",
  icon: "size-8 rounded-lg [&_svg]:size-4",
  "icon-sm": "size-7 rounded-md [&_svg]:size-4",
  "icon-xs": "size-6 rounded-md [&_svg]:size-3.5",
};

export function Button({
  variant = "secondary",
  size = "md",
  icon,
  loading,
  className,
  children,
  disabled,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: Variant;
  size?: Size;
  icon?: ReactNode;
  loading?: boolean;
}) {
  return (
    <button
      {...rest}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      className={clsx(
        "inline-flex shrink-0 cursor-pointer items-center justify-center border font-normal whitespace-nowrap select-none",
        "[&_svg]:pointer-events-none [&_svg]:shrink-0",
        "transition-[background-color,border-color,color,box-shadow,transform] duration-100 active:scale-[.985]",
        "outline-none focus-visible:ring-3 focus-visible:ring-ring/50",
        "aria-[invalid=true]:border-destructive aria-[invalid=true]:ring-3 aria-[invalid=true]:ring-destructive/20 dark:aria-[invalid=true]:ring-destructive/40",
        "disabled:cursor-not-allowed disabled:opacity-50 disabled:active:scale-100",
        variants[variant],
        sizes[size],
        className,
      )}
    >
      {loading ? <Spinner /> : icon}
      {children}
    </button>
  );
}

/** Kobra spinner: a 16px arc in the current text colour. */
export function Spinner({ className }: { className?: string }) {
  return (
    <svg
      aria-hidden
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      className={clsx("size-4 shrink-0 animate-spin", className)}
    >
      {/* Three quarters of a circle, r = 6.25, open at the top right. */}
      <path d="M14.25 8A6.25 6.25 0 1 1 8 1.75" />
    </svg>
  );
}
