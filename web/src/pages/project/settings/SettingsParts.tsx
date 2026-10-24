import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";

/** A settings section: heading, optional explanation, then its content. */
export function SettingsSection({ title, description, children }: { title: string; description?: ReactNode; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-4">
      <div>
        <h2 className="text-lg font-semibold">{title}</h2>
        {description ? <p className="mt-1 text-sm text-muted-foreground">{description}</p> : null}
      </div>
      {children}
    </section>
  );
}

/** Inline "type the name to confirm" row. */
export function ConfirmDelete({
  name,
  label,
  pending,
  onConfirm,
  onCancel,
}: {
  name: string;
  label: string;
  pending: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const [typed, setTyped] = useState("");
  return (
    <form
      className="flex flex-col gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        if (typed === name) onConfirm();
      }}
    >
      <Field label={`Type "${name}" to confirm`}>
        {(p) => <Input {...p} autoFocus autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />}
      </Field>
      <div className="flex justify-end gap-2">
        <Button type="button" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" variant="danger" loading={pending} disabled={typed !== name}>
          {label}
        </Button>
      </div>
    </form>
  );
}
