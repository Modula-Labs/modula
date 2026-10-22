import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import type { ReactNode } from "react";

export function Modal({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  width = 440,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  width?: number;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-[hsl(var(--overlay))] backdrop-blur-[2px]" />
        <Dialog.Content
          style={{ width }}
          className="anim-modal fixed top-[18%] left-1/2 z-50 max-w-[calc(100vw-32px)] -translate-x-1/2 rounded-lg bg-surface-2 p-6 shadow-modal"
        >
          <div className="mb-4 flex items-start justify-between gap-4">
            <div>
              <Dialog.Title className="text-lg font-semibold">{title}</Dialog.Title>
              {description ? (
                <Dialog.Description className="mt-1 text-sm text-fg-muted">{description}</Dialog.Description>
              ) : (
                <Dialog.Description className="sr-only">{title}</Dialog.Description>
              )}
            </div>
            <Dialog.Close className="rounded-md p-1 text-fg-muted hover:bg-surface-3 hover:text-fg" aria-label="Close">
              <X className="size-4" />
            </Dialog.Close>
          </div>
          {children}
          {footer ? <div className="mt-6 flex justify-end gap-2">{footer}</div> : null}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
