import * as M from "@radix-ui/react-dropdown-menu";
import { clsx } from "clsx";
import type { ReactNode } from "react";

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
