import * as M from "@radix-ui/react-dropdown-menu";
import { clsx } from "clsx";
import { createContext, useContext, useState, type ReactNode } from "react";

/** Kobra menus come in two densities. Default (dropdowns): radius xl panel with 6px
 *  inset, 32px rows, 10x6 padding, 16px glyphs. SM (context menus): radius lg panel
 *  with 4px inset, 28px rows, 6x4 padding. Both: shadow-md + 1px foreground ring. */
type Density = "default" | "sm";
const DensityContext = createContext<Density>("default");

const panel = "z-50 bg-popover text-popover-foreground shadow-menu outline-none";

export function Menu({ trigger, children, align = "end" }: { trigger: ReactNode; children: ReactNode; align?: "start" | "end" }) {
  return (
    <M.Root>
      <M.Trigger asChild>{trigger}</M.Trigger>
      <M.Portal>
        <M.Content align={align} sideOffset={4} collisionPadding={8} className={clsx("anim-pop min-w-48 rounded-xl p-1.5", panel)}>
          <DensityContext.Provider value="default">{children}</DensityContext.Provider>
        </M.Content>
      </M.Portal>
    </M.Root>
  );
}

export function MenuItem({
  onSelect,
  danger,
  icon,
  children,
  disabled,
  title,
  shortcut,
}: {
  onSelect: () => void;
  danger?: boolean;
  icon?: ReactNode;
  children: ReactNode;
  disabled?: boolean;
  title?: string;
  /** Keyboard hint drawn muted at the row's end, e.g. "⇧⌘P". */
  shortcut?: string;
}) {
  const sm = useContext(DensityContext) === "sm";
  return (
    <M.Item
      title={title}
      disabled={disabled}
      onSelect={onSelect}
      className={clsx(
        "flex cursor-default items-center text-sm outline-none select-none [&_svg]:size-4 [&_svg]:shrink-0",
        sm ? "min-h-7 gap-1.5 rounded-md px-1.5 py-1" : "min-h-8 gap-2 rounded-lg px-2.5 py-1.5",
        "data-[disabled]:pointer-events-none data-[disabled]:opacity-50",
        danger ? "text-destructive data-[highlighted]:bg-destructive/10" : "text-foreground data-[highlighted]:bg-accent",
      )}
    >
      {icon}
      <span className="min-w-0 flex-1">{children}</span>
      {shortcut ? <span className="ml-auto pl-2 text-xs tracking-[1.2px] text-muted-foreground">{shortcut}</span> : null}
    </M.Item>
  );
}

export function MenuSeparator() {
  const sm = useContext(DensityContext) === "sm";
  return <M.Separator className={clsx("h-px bg-border", sm ? "my-1" : "my-1.5")} />;
}

export function MenuLabel({ children }: { children: ReactNode }) {
  const sm = useContext(DensityContext) === "sm";
  return (
    <M.Label className={clsx("py-1 text-xs text-muted-foreground select-none", sm ? "px-1.5" : "px-2.5")}>{children}</M.Label>
  );
}

/** A menu opened at a point (right-click). `at` is in viewport pixels; null
 *  closes it. Keyboard navigable; closes on Esc and outside clicks; flips to
 *  stay inside the viewport; grows from the cursor. */
export function ContextMenu({
  at,
  onClose,
  label,
  children,
}: {
  at: { x: number; y: number } | null;
  onClose: () => void;
  /** Accessible name of the menu. */
  label: string;
  children: ReactNode;
}) {
  // Keep the last point while the menu animates out.
  const [last, setLast] = useState(at);
  if (at && (at.x !== last?.x || at.y !== last?.y)) setLast(at);
  return (
    <M.Root open={!!at} onOpenChange={(o) => !o && onClose()} modal={false}>
      <M.Trigger asChild>
        <span aria-hidden style={{ position: "fixed", left: last?.x ?? 0, top: last?.y ?? 0, width: 0, height: 0 }} />
      </M.Trigger>
      <M.Portal>
        <M.Content
          aria-label={label}
          side="bottom"
          align="start"
          sideOffset={2}
          collisionPadding={8}
          onCloseAutoFocus={(e) => e.preventDefault()}
          className={clsx("anim-menu min-w-36 rounded-lg p-1", panel)}
        >
          <DensityContext.Provider value="sm">{children}</DensityContext.Provider>
        </M.Content>
      </M.Portal>
    </M.Root>
  );
}
