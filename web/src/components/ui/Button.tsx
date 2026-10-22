import { clsx } from "clsx";
import type { ButtonHTMLAttributes, ReactNode } from "react";

type Variant = "primary" | "secondary" | "outline-accent" | "ghost" | "danger" | "danger-outline";
type Size = "sm" | "md" | "lg";

const variants: Record<Variant, string> = {
  primary: "border-transparent bg-accent text-accent-fg hover:bg-accent-hover",
  secondary: "border-border bg-surface-3 text-fg hover:border-border-strong",
  "outline-accent": "border-accent bg-transparent text-accent-text hover:bg-accent-subtle",
  ghost: "border-transparent text-fg-muted hover:bg-surface-3 hover:text-fg",
  danger: "border-transparent bg-danger-solid text-white hover:brightness-110",
  "danger-outline": "border-danger/60 bg-transparent text-danger hover:bg-danger-subtle",
};
const sizes: Record<Size, string> = { sm: "h-7 px-2.5", md: "h-8 px-3", lg: "h-9 px-4" };

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
        "inline-flex items-center justify-center gap-2 rounded-sm border text-sm font-medium whitespace-nowrap",
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
      className={clsx("inline-block size-4 animate-spin rounded-full border-2 border-current border-r-transparent", className)}
    />
  );
}
