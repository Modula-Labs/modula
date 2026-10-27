import { type ReactNode } from "react";
import { Field, Input } from "@/components/ui/Input";
import { serviceName, slugifyServiceName, validServiceName } from "@/lib/templates";

/** Why `name` can't be used (null: it can). Empty is fine when there's a default. */
export function serviceNameError(name: string, taken: readonly string[] = []): string | null {
  if (name === "") return null;
  if (!validServiceName(name)) return name === "shared" ? "\"shared\" is reserved for shared variables." : "Must start with a letter.";
  if (taken.includes(name)) return `A service named ${name} already exists.`;
  return null;
}

/**
 * A service name field: what's typed becomes a service name as you type
 * ("Postgres API" → `postgres-api`), with the hostname and reference it gives.
 * `value` is the raw field text; submit {@link serviceName}(value) (or the
 * placeholder when empty).
 */
export function ServiceNameField({
  value,
  onChange,
  placeholder,
  taken,
  label = "Service name",
  hint,
  autoFocus,
  disabled,
  onEnter,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  taken?: readonly string[];
  label?: string;
  hint?: ReactNode;
  autoFocus?: boolean;
  disabled?: boolean;
  onEnter?: () => void;
}) {
  const name = serviceName(value) || placeholder || "";
  const error = serviceNameError(serviceName(value), taken);
  return (
    <Field
      label={label}
      error={error ?? undefined}
      hint={
        hint ??
        (name ? (
          <>
            Reached at <span className="font-mono">{name}.modula.internal</span>; referenced as{" "}
            <span className="font-mono">{`\${{${name}.NAME}}`}</span>.
          </>
        ) : undefined)
      }
    >
      {(p) => (
        <Input
          {...p}
          mono
          maxLength={63}
          autoFocus={autoFocus}
          disabled={disabled}
          placeholder={placeholder}
          value={value}
          onChange={(e) => onChange(slugifyServiceName(e.target.value))}
          onKeyDown={(e) => {
            if (e.key === "Enter" && onEnter) {
              e.preventDefault();
              onEnter();
            }
          }}
        />
      )}
    </Field>
  );
}
