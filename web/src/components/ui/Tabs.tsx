import * as T from "@radix-ui/react-tabs";
import { clsx } from "clsx";
import type { ReactNode } from "react";

/* Kobra Tabs. Triggers are radius md, 14px regular text, foreground at 60% until
   hovered or active. "line" (sheets) draws a 2px foreground bar under the active
   trigger; "default" sits in a 32px muted list with a 3px inset and lifts the
   active trigger onto the background with shadow-sm. */

const trigger =
  "inline-flex h-[26px] items-center justify-center gap-1.5 rounded-md px-1.5 text-sm whitespace-nowrap text-foreground/60 transition-colors outline-none hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring disabled:pointer-events-none disabled:opacity-50";

/** Default-variant active trigger (mirrored as data-state classes in Tabs). Dark mode
 *  lifts it with a faint wash instead of the (darker) page background. */
const lifted = "bg-background text-foreground shadow-sm dark:bg-foreground/10 dark:shadow-none";

/** Tabs for sheets (service, deployment). Controlled by the caller. */
export function Tabs<V extends string>({
  value,
  onChange,
  tabs,
  children,
  variant = "line",
}: {
  value: V;
  onChange: (v: V) => void;
  tabs: { value: V; label: string }[];
  children: ReactNode;
  variant?: "line" | "default";
}) {
  const line = variant === "line";
  return (
    <T.Root value={value} onValueChange={(v) => onChange(v as V)} className="flex min-h-0 flex-1 flex-col">
      {/* The hairline under the line list is ours: it separates the sheet header from its panel. */}
      <div className={clsx("px-6 md:px-10", line && "border-b border-border")}>
        <T.List className={clsx("flex h-8 w-fit items-center p-[3px]", line ? "-ml-[9px] gap-1" : "rounded-lg bg-muted")}>
          {tabs.map((t) => (
            <T.Trigger
              key={t.value}
              value={t.value}
              className={clsx(
                trigger,
                line
                  ? "relative data-[state=active]:text-foreground data-[state=active]:after:absolute data-[state=active]:after:inset-x-0 data-[state=active]:after:-bottom-[4px] data-[state=active]:after:h-0.5 data-[state=active]:after:bg-foreground"
                  : "data-[state=active]:bg-background data-[state=active]:text-foreground data-[state=active]:shadow-sm dark:data-[state=active]:bg-foreground/10 dark:data-[state=active]:shadow-none",
              )}
            >
              {t.label}
            </T.Trigger>
          ))}
        </T.List>
      </div>
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

/** Kobra Tabs (default variant) as a radio group, for small toggles. */
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
    <div role="radiogroup" aria-label={label} className="inline-flex h-8 items-center rounded-lg bg-muted p-[3px]">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={o.value === value}
          onClick={() => onChange(o.value)}
          className={clsx(trigger, o.value === value && lifted)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
