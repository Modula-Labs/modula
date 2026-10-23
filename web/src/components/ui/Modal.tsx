import * as Dialog from "@radix-ui/react-dialog";
import { IconX } from "@tabler/icons-react";
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
        {/* The overlay scrolls, so a dialog taller than the window stays reachable. */}
        <Dialog.Overlay className="fixed inset-0 z-40 flex items-start justify-center overflow-y-auto bg-black/60 px-4 pt-[min(18vh,10rem)] pb-8 backdrop-blur-[2px]">
          <Dialog.Content
            style={{ width }}
            className="anim-modal relative z-50 max-w-full rounded-xl bg-popover p-6 text-popover-foreground shadow-modal"
          >
            <div className="mb-4 flex items-start justify-between gap-4">
              <div>
                <Dialog.Title className="text-lg font-semibold">
                  {title}
                </Dialog.Title>
                {description ? (
                  <Dialog.Description className="mt-1 text-sm text-muted-foreground">
                    {description}
                  </Dialog.Description>
                ) : (
                  <Dialog.Description className="sr-only">
                    {title}
                  </Dialog.Description>
                )}
              </div>
              <Dialog.Close
                className="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
                aria-label="Close"
              >
                <IconX className="size-4" stroke={1.8} />
              </Dialog.Close>
            </div>
            {children}
            {footer ? (
              <div className="mt-6 flex justify-end gap-2">{footer}</div>
            ) : null}
          </Dialog.Content>
        </Dialog.Overlay>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
