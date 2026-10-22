import { clsx } from "clsx";
import { forwardRef, useId, type InputHTMLAttributes, type ReactNode } from "react";

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement> & { mono?: boolean }>(
  function Input({ className, mono, ...rest }, ref) {
    return (
      <input
        ref={ref}
        {...rest}
        className={clsx(
          "h-9 w-full rounded-sm border border-border-strong bg-surface px-3 text-sm text-fg placeholder:text-fg-subtle",
          "transition-[border-color,box-shadow] duration-100 focus:border-accent focus:outline-none",
          "aria-[invalid=true]:border-danger",
          mono && "font-mono text-[13px]",
          className,
        )}
      />
    );
  },
);

export function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  children: (props: { id: string; "aria-invalid": boolean; "aria-describedby"?: string }) => ReactNode;
}) {
  const id = useId();
  const describedBy = error ? `${id}-err` : hint ? `${id}-hint` : undefined;
  return (
    <div className="flex flex-col gap-1.5">
      <label htmlFor={id} className="text-sm font-medium text-fg">
        {label}
      </label>
      {children({ id, "aria-invalid": !!error, "aria-describedby": describedBy })}
      {error ? (
        <p id={`${id}-err`} className="text-xs text-danger" role="alert">
          {error}
        </p>
      ) : hint ? (
        <p id={`${id}-hint`} className="text-xs text-fg-subtle">
          {hint}
        </p>
      ) : null}
    </div>
  );
}
