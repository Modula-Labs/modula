import { useEffect, useRef } from "react";

/** Six single-digit boxes: auto-advance, backspace goes back, paste fills all. */
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
    <div className="flex gap-2" role="group" aria-label="6-digit code">
      {digits.map((d, i) => (
        <input
          key={i}
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
          className="h-[52px] w-11 rounded-sm border border-input bg-card text-center font-mono text-[22px] text-foreground focus:border-ring focus:outline-none aria-[invalid=true]:border-destructive"
        />
      ))}
    </div>
  );
}
