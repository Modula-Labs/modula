import * as M from "@radix-ui/react-dropdown-menu";
import { clsx } from "clsx";
import { useState, type ReactNode } from "react";

export function Menu({ trigger, children, align = "end" }: { trigger: ReactNode; children: ReactNode; align?: "start" | "end" }) {
  return (
    <M.Root>
      <M.Trigger asChild>{trigger}</M.Trigger>
      <M.Portal>
        <M.Content
          align={align}
          sideOffset={4}
          className="anim-pop z-50 min-w-44 rounded-lg bg-popover p-1 text-popover-foreground shadow-menu"
        >
          {children}
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
}: {
  onSelect: () => void;
  danger?: boolean;
  icon?: ReactNode;
  children: ReactNode;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <M.Item
      title={title}
      disabled={disabled}
      onSelect={onSelect}
      className={clsx(
        "flex h-8 cursor-default items-center gap-2 rounded-md px-2 text-sm outline-none select-none [&_svg]:size-4",
        "data-[disabled]:opacity-50 data-[highlighted]:bg-accent",
        danger ? "text-error" : "text-foreground [&_svg]:text-muted-foreground",
      )}
    >
      {icon}
      {children}
    </M.Item>
  );
}

export function MenuSeparator() {
  return <M.Separator className="my-1 h-px bg-border" />;
}

export function MenuLabel({ children }: { children: ReactNode }) {
  return <M.Label className="px-2 pt-1.5 pb-1 text-xs font-medium text-muted-foreground select-none">{children}</M.Label>;
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
          className="anim-menu z-50 min-w-52 rounded-lg bg-popover p-1 text-popover-foreground shadow-menu"
        >
          {children}
        </M.Content>
      </M.Portal>
    </M.Root>
  );
}
