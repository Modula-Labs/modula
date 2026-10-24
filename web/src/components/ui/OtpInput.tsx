import { clsx } from "clsx";
import { Fragment, useEffect, useRef } from "react";

/**
 * Six single-digit boxes: auto-advance, backspace goes back, paste fills all.
 * Kobra Input OTP: two groups of three split by a short rule; each slot is radius
 * xl on a 6% foreground wash with a faint 0 when empty; the active slot darkens to
 * 10% with a 3px ring (foreground 75%, destructive when invalid). Slots are 44px
 * rather than Kobra's 56 so the row fits the 336px auth card.
 */
export function OtpInput({
  value,
  onChange,
  onComplete,
  disabled,
  autoFocus,
  invalid,
}: {
  value: string;
  onChange: (v: string) => void;
  onComplete?: (v: string) => void;
  disabled?: boolean;
  autoFocus?: boolean;
  invalid?: boolean;
}) {
  const refs = useRef<(HTMLInputElement | null)[]>([]);
  // After a rejected code the boxes are cleared; put the caret back at the start.
  useEffect(() => {
    if (invalid && !value) refs.current[0]?.focus();
  }, [invalid, value]);
  const digits = Array.from({ length: 6 }, (_, i) => value[i] ?? "");

  const set = (next: string) => {
    const clean = next.replace(/\D/g, "").slice(0, 6);
    onChange(clean);
    if (clean.length === 6) onComplete?.(clean);
    return clean;
  };

  return (
    <div className="flex items-center gap-2" role="group" aria-label="6-digit code">
      {digits.map((d, i) => (
        <Fragment key={i}>
          {i === 3 ? <span aria-hidden className="h-0.5 w-3 shrink-0 rounded-full bg-muted-foreground" /> : null}
          <input
            ref={(el) => {
              refs.current[i] = el;
            }}
            value={d}
            disabled={disabled}
            autoFocus={autoFocus && i === 0}
            inputMode="numeric"
            autoComplete={i === 0 ? "one-time-code" : "off"}
            aria-label={`Digit ${i + 1}`}
            aria-invalid={invalid}
            maxLength={6}
            onChange={(e) => {
              const typed = e.target.value.replace(/\D/g, "");
              if (!typed) return;
              if (typed.length > 1) {
                const filled = set(value.slice(0, i) + typed);
                refs.current[Math.min(filled.length, 5)]?.focus();
                return;
              }
              set(value.slice(0, i) + typed + value.slice(i + 1));
              refs.current[i + 1]?.focus();
            }}
            onKeyDown={(e) => {
              if (e.key === "Backspace") {
                e.preventDefault();
                if (d) set(value.slice(0, i) + value.slice(i + 1));
                else if (i > 0) {
                  set(value.slice(0, i - 1) + value.slice(i));
                  refs.current[i - 1]?.focus();
                }
              } else if (e.key === "ArrowLeft" && i > 0) refs.current[i - 1]?.focus();
              else if (e.key === "ArrowRight" && i < 5) refs.current[i + 1]?.focus();
            }}
            onPaste={(e) => {
              e.preventDefault();
              const filled = set(e.clipboardData.getData("text"));
              refs.current[Math.min(filled.length, 5)]?.focus();
            }}
            placeholder="0"
            className={clsx(
              "size-11 shrink-0 rounded-xl border border-foreground/8 bg-foreground/6 text-center text-2xl font-bold text-foreground tabular-nums",
              "placeholder:text-foreground/20 outline-none transition-[background-color,box-shadow] duration-100",
              "focus:bg-foreground/10 focus:ring-3 focus:ring-foreground/75 focus:ring-inset",
              "aria-[invalid=true]:border-destructive/50 aria-[invalid=true]:focus:ring-destructive",
              "disabled:cursor-not-allowed disabled:opacity-50",
            )}
          />
        </Fragment>
      ))}
    </div>
  );
}
