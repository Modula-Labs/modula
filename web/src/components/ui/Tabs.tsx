import * as T from "@radix-ui/react-tabs";
import type { ReactNode } from "react";

/** Underline tabs (service sheet, deployment sheet). Controlled by the caller. */
export function Tabs<V extends string>({
  value,
  onChange,
  tabs,
  children,
}: {
  value: V;
  onChange: (v: V) => void;
  tabs: { value: V; label: string }[];
  children: ReactNode;
}) {
  return (
    <T.Root value={value} onValueChange={(v) => onChange(v as V)} className="flex min-h-0 flex-1 flex-col">
      <T.List className="flex gap-5 border-b border-border px-6 md:px-10">
        {tabs.map((t) => (
          <T.Trigger
            key={t.value}
            value={t.value}
            className="-mb-px border-b-2 border-transparent pb-2.5 text-[15px] text-muted-foreground hover:text-foreground data-[state=active]:border-foreground data-[state=active]:text-foreground"
          >
            {t.label}
          </T.Trigger>
        ))}
      </T.List>
      {children}
    </T.Root>
  );
}

export function TabPanel({ value, children }: { value: string; children: ReactNode }) {
  return (
    <T.Content value={value} className="min-h-0 flex-1 overflow-auto px-6 py-6 focus-visible:outline-none md:px-10">
      {children}
    </T.Content>
  );
}

/** Pill segmented control (small toggles). */
export function Segmented<V extends string>({
  value,
  onChange,
  options,
  label,
}: {
  value: V;
  onChange: (v: V) => void;
  options: { value: V; label: ReactNode }[];
  label: string;
}) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex rounded-full bg-muted p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          role="radio"
          aria-checked={o.value === value}
          onClick={() => onChange(o.value)}
          className="rounded-full px-3 py-1 text-[13px] text-muted-foreground aria-checked:bg-card aria-checked:text-foreground aria-checked:shadow-raise"
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
