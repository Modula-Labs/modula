import { clsx } from "clsx";
import type { ButtonHTMLAttributes, ReactNode } from "react";

/** Kobra button. `primary` is the one teal action per screen; everything else is neutral. */
type Variant = "primary" | "secondary" | "outline" | "ghost" | "danger" | "danger-outline";
type Size = "sm" | "md" | "lg" | "icon";

const variants: Record<Variant, string> = {
  primary: "border-transparent bg-primary text-primary-foreground shadow-raise hover:bg-primary/90",
  secondary: "border-border bg-secondary text-secondary-foreground shadow-raise hover:bg-accent",
  outline: "border-input bg-transparent text-foreground hover:bg-accent",
  ghost: "border-transparent text-muted-foreground hover:bg-accent hover:text-foreground",
  danger: "border-transparent bg-destructive text-destructive-foreground shadow-raise hover:bg-destructive/90",
  "danger-outline": "border-destructive/50 bg-transparent text-error hover:bg-destructive/10",
};
const sizes: Record<Size, string> = {
  sm: "h-8 px-2.5 text-[13px]",
  md: "h-9 px-3.5",
  lg: "h-10 px-4",
  icon: "size-9",
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
      className={clsx(
        "inline-flex items-center justify-center gap-2 rounded-md border text-sm font-medium whitespace-nowrap [&_svg]:size-4",
        "transition-[background-color,border-color,transform] duration-100 active:scale-[.98]",
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

export function Spinner({ className }: { className?: string }) {
  return (
    <span
      aria-hidden
      className={clsx(
        "inline-block size-4 animate-spin rounded-full border-2 border-current border-r-transparent",
        className,
      )}
    />
  );
}
