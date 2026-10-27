import { clsx } from "clsx";
import {
  forwardRef,
  useId,
  type InputHTMLAttributes,
  type ReactNode,
  type SelectHTMLAttributes,
  type TextareaHTMLAttributes,
} from "react";

/**
 * Kobra field chrome shared by Input, Textarea and Select: radius lg, 1px input
 * border, transparent fill. Focus is the 2px ring at a 1px offset (border stays);
 * invalid swaps the border to destructive and adds a 3px ring at 20%; disabled is
 * 50% over a light wash.
 */
export const controlClass = clsx(
  "w-full min-w-0 rounded-lg border border-input bg-transparent text-sm text-foreground dark:bg-input/30",
  "placeholder:text-muted-foreground transition-[border-color,box-shadow,outline-color] duration-100",
  "aria-[invalid=true]:border-destructive aria-[invalid=true]:ring-3 aria-[invalid=true]:ring-destructive/20",
  "dark:aria-[invalid=true]:ring-destructive/40 aria-[invalid=true]:focus-visible:outline-destructive/60",
  "disabled:cursor-not-allowed disabled:bg-input/50 disabled:opacity-50 dark:disabled:bg-input/80",
);

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement> & { mono?: boolean }>(
  function Input({ className, mono, ...rest }, ref) {
    return (
      <input
        ref={ref}
        {...rest}
        className={clsx(controlClass, "h-8 px-2.5 py-1", mono && "font-mono text-[13px]", className)}
      />
    );
  },
);

/**
 * Multi-line field: 64px minimum, 10 by 8 padding. Mono by default because every
 * current caller edits code-like text (variables, keys, rows); pass `mono={false}`
 * for prose.
 */
export const Textarea = forwardRef<HTMLTextAreaElement, TextareaHTMLAttributes<HTMLTextAreaElement> & { mono?: boolean }>(
  function Textarea({ className, mono = true, ...rest }, ref) {
    return (
      <textarea
        ref={ref}
        {...rest}
        className={clsx(controlClass, "min-h-16 px-2.5 py-2", mono && "font-mono text-[13px]", className)}
      />
    );
  },
);

/**
 * Native select styled as Kobra's Select Trigger. The muted 16px chevron is a
 * background image (light and dark) so width classes on the select keep working.
 */
const selectChevron =
  `bg-[url("data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20viewBox='0%200%2016%2016'%20fill='none'%20stroke='%23737373'%20stroke-width='1.8'%20stroke-linecap='round'%20stroke-linejoin='round'%3E%3Cpath%20d='M4%206l4%204%204-4'/%3E%3C/svg%3E")] ` +
  `dark:bg-[url("data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20viewBox='0%200%2016%2016'%20fill='none'%20stroke='%23a1a1a1'%20stroke-width='1.8'%20stroke-linecap='round'%20stroke-linejoin='round'%3E%3Cpath%20d='M4%206l4%204%204-4'/%3E%3C/svg%3E")] ` +
  "bg-size-[16px] bg-position-[right_10px_center] bg-no-repeat";

export function Select({ className, children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select
      {...rest}
      className={clsx(controlClass, selectChevron, "h-8 cursor-pointer appearance-none pr-8 pl-2.5 [&_option]:bg-popover [&_option]:text-popover-foreground", className)}
    >
      {children}
    </select>
  );
}

/** Label, control, description and error, 8px apart. Invalid turns label and error destructive. */
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
  const describedBy = [hint ? `${id}-hint` : null, error ? `${id}-err` : null].filter(Boolean).join(" ") || undefined;
  return (
    <div className="flex flex-col gap-2" data-invalid={error ? true : undefined}>
      <label
        htmlFor={id}
        className={clsx("text-sm leading-none font-normal select-none", error ? "text-destructive" : "text-foreground")}
      >
        {label}
      </label>
      {children({ id, "aria-invalid": !!error, "aria-describedby": describedBy })}
      {hint ? (
        <p id={`${id}-hint`} className="text-sm text-muted-foreground">
          {hint}
        </p>
      ) : null}
      {error ? (
        <p id={`${id}-err`} className="text-sm text-destructive" role="alert">
          {error}
        </p>
      ) : null}
    </div>
  );
}
