import { clsx } from "clsx";
import {
  createContext,
  forwardRef,
  useContext,
  useEffect,
  useImperativeHandle,
  useRef,
  type ButtonHTMLAttributes,
  type InputHTMLAttributes,
  type ReactNode,
} from "react";
import { Check, Minus } from "./icons";

/*
 * Kobra checkbox, radio and switch. Plain native controls (a styled
 * <input type="checkbox|radio"> and a <button role="switch">), so they work in
 * forms and with the keyboard as is. Checked fills with --foreground and draws the
 * tick, dot or knob in --background (project rule: never teal).
 */

/** Focus: ring-coloured border plus a 3px ring at 50%. Invalid: destructive border and a 20% ring. */
const ring = clsx(
  "outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50",
  "aria-[invalid=true]:border-destructive aria-[invalid=true]:ring-3 aria-[invalid=true]:ring-destructive/20 dark:aria-[invalid=true]:ring-destructive/40",
);

/** Optional label row: control and text 8px apart, 14/14 regular. */
function withLabel(control: ReactNode, label: ReactNode | undefined, disabled: boolean | undefined, className?: string) {
  if (label == null) return control;
  return (
    <label
      className={clsx(
        "inline-flex items-center gap-2 text-sm leading-none text-foreground select-none",
        disabled ? "cursor-not-allowed" : "cursor-pointer",
        className,
      )}
    >
      {control}
      <span className={clsx(disabled && "opacity-50")}>{label}</span>
    </label>
  );
}

type CheckboxProps = Omit<InputHTMLAttributes<HTMLInputElement>, "type"> & {
  onCheckedChange?: (checked: boolean) => void;
  /** Shows a dash instead of the tick (e.g. a partly selected "select all"). */
  indeterminate?: boolean;
  /** Kobra's Round shape: a circle with the same tick. */
  round?: boolean;
  /** Renders the checkbox inside a <label> with this text beside it. */
  label?: ReactNode;
};

/** 16px box, radius 4, 1px input border; checked fills with foreground. */
export const Checkbox = forwardRef<HTMLInputElement, CheckboxProps>(function Checkbox(
  { onCheckedChange, onChange, indeterminate, round, label, className, disabled, ...rest },
  ref,
) {
  const inner = useRef<HTMLInputElement>(null);
  useImperativeHandle(ref, () => inner.current as HTMLInputElement);
  useEffect(() => {
    if (inner.current) inner.current.indeterminate = !!indeterminate;
  }, [indeterminate]);

  const control = (
    <span className={clsx("relative inline-flex size-4 shrink-0", label == null && className)}>
      <input
        ref={inner}
        type="checkbox"
        disabled={disabled}
        {...rest}
        onChange={(e) => {
          onChange?.(e);
          onCheckedChange?.(e.target.checked);
        }}
        className={clsx(
          "peer size-4 shrink-0 cursor-pointer appearance-none border border-input bg-transparent shadow-none dark:bg-input/30",
          round ? "rounded-full" : "rounded-checkbox",
          "transition-[background-color,border-color,box-shadow] duration-100",
          "checked:border-foreground checked:bg-foreground indeterminate:border-foreground indeterminate:bg-foreground",
          "disabled:cursor-not-allowed disabled:opacity-50",
          ring,
        )}
      />
      <Check
        aria-hidden
        className="pointer-events-none absolute inset-0 m-auto hidden size-3 text-background peer-checked:block peer-indeterminate:hidden"
      />
      <Minus
        aria-hidden
        className="pointer-events-none absolute inset-0 m-auto hidden size-3 text-background peer-indeterminate:block"
      />
    </span>
  );
  return withLabel(control, label, disabled, className);
});

type SwitchProps = Omit<ButtonHTMLAttributes<HTMLButtonElement>, "onChange"> & {
  checked: boolean;
  onCheckedChange?: (checked: boolean) => void;
  /** Renders the switch inside a <label> with this text beside it. */
  label?: ReactNode;
};

/** 33x18 track, radius full; 14px knob 2px in from either end. Off = input colour, on = foreground. */
export const Switch = forwardRef<HTMLButtonElement, SwitchProps>(function Switch(
  { checked, onCheckedChange, onClick, label, className, disabled, ...rest },
  ref,
) {
  const control = (
    <button
      ref={ref}
      type="button"
      role="switch"
      aria-checked={checked}
      data-state={checked ? "checked" : "unchecked"}
      disabled={disabled}
      {...rest}
      onClick={(e) => {
        onClick?.(e);
        if (!e.defaultPrevented) onCheckedChange?.(!checked);
      }}
      className={clsx(
        "group/switch inline-flex h-[18px] w-[33px] shrink-0 cursor-pointer items-center rounded-full border border-transparent p-px",
        "transition-[background-color,border-color,box-shadow] duration-150",
        checked ? "bg-foreground" : "bg-input dark:bg-input/80",
        "disabled:cursor-not-allowed disabled:opacity-50",
        ring,
        label == null && className,
      )}
    >
      <span
        aria-hidden
        className={clsx(
          "pointer-events-none block size-3.5 rounded-full shadow-sm transition-transform duration-150 ease-out",
          checked ? "translate-x-[15px] bg-background" : "translate-x-0 bg-background dark:bg-foreground",
        )}
      />
    </button>
  );
  return withLabel(control, label, disabled, className);
});

type RadioGroupContext = { name?: string; value?: string; onValueChange?: (value: string) => void; disabled?: boolean };
const RadioCtx = createContext<RadioGroupContext | null>(null);

/** Stacks Radio options 8px apart and owns their name and selected value. */
export function RadioGroup({
  name,
  value,
  onValueChange,
  disabled,
  orientation = "vertical",
  className,
  children,
  ...rest
}: RadioGroupContext & {
  orientation?: "vertical" | "horizontal";
  className?: string;
  children: ReactNode;
  "aria-label"?: string;
  "aria-labelledby"?: string;
  "aria-invalid"?: boolean;
}) {
  return (
    <RadioCtx.Provider value={{ name, value, onValueChange, disabled }}>
      <div
        role="radiogroup"
        {...rest}
        className={clsx("flex gap-2", orientation === "vertical" ? "flex-col" : "flex-row flex-wrap gap-x-4", className)}
      >
        {children}
      </div>
    </RadioCtx.Provider>
  );
}

type RadioProps = Omit<InputHTMLAttributes<HTMLInputElement>, "type" | "value"> & {
  value: string;
  /** Renders the radio inside a <label> with this text beside it. */
  label?: ReactNode;
};

/** 16px circle; the input border does not change when selected; 8px foreground dot. */
export const Radio = forwardRef<HTMLInputElement, RadioProps>(function Radio(
  { value, label, className, disabled, checked, name, onChange, ...rest },
  ref,
) {
  const group = useContext(RadioCtx);
  const off = disabled ?? group?.disabled;
  const control = (
    <span className={clsx("relative inline-flex size-4 shrink-0", label == null && className)}>
      <input
        ref={ref}
        type="radio"
        value={value}
        name={name ?? group?.name}
        disabled={off}
        checked={checked ?? (group?.value !== undefined ? group.value === value : undefined)}
        {...rest}
        onChange={(e) => {
          onChange?.(e);
          if (e.target.checked) group?.onValueChange?.(value);
        }}
        className={clsx(
          "peer size-4 shrink-0 cursor-pointer appearance-none rounded-full border border-input bg-transparent dark:bg-input/30",
          "transition-[border-color,box-shadow] duration-100 disabled:cursor-not-allowed disabled:opacity-50",
          ring,
        )}
      />
      <span
        aria-hidden
        className="pointer-events-none absolute inset-0 m-auto hidden size-2 rounded-full bg-foreground peer-checked:block peer-disabled:opacity-50"
      />
    </span>
  );
  return withLabel(control, label, off, className);
});
