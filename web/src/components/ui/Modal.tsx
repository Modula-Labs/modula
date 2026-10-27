import * as Dialog from "@radix-ui/react-dialog";
import { IconX } from "@tabler/icons-react";
import type { ReactNode } from "react";

/** Kobra dialog widths: Default 384 · SM 512 · MD 672 · LG 768 · XL 896. */
export const MODAL_WIDTHS = { default: 384, sm: 512, md: 672, lg: 768, xl: 896 } as const;

/** Kobra Dialog: radius 3xl, 28px inset (20 at the bottom), 16px between header,
 *  content and footer; shadow-2xl + 1px foreground ring; 32px round close button. */
export function Modal({
  open,
  onOpenChange,
  title,
  description,
  children,
  footer,
  width = MODAL_WIDTHS.sm,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  description?: ReactNode;
  children?: ReactNode;
  footer?: ReactNode;
  /** Pixel width; prefer one of MODAL_WIDTHS. */
  width?: number;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        {/* The overlay scrolls, so a dialog taller than the window stays reachable. */}
        <Dialog.Overlay className="fixed inset-0 z-[60] flex items-start justify-center overflow-y-auto bg-overlay px-4 pt-[min(18vh,10rem)] pb-8 backdrop-blur-[2px]">
          <Dialog.Content
            style={{ width }}
            className="anim-modal relative flex max-w-full flex-col gap-4 rounded-3xl bg-popover px-7 pt-7 pb-5 text-popover-foreground shadow-modal outline-none"
          >
            <div className="flex flex-col gap-2 pr-10">
              <Dialog.Title className="text-lg leading-[18px] font-bold tracking-[-0.45px]">{title}</Dialog.Title>
              {description ? (
                <Dialog.Description className="text-sm text-muted-foreground">{description}</Dialog.Description>
              ) : (
                <Dialog.Description className="sr-only">{title}</Dialog.Description>
              )}
            </div>
            {/* Close stays before the content in the DOM so focus order is unchanged. */}
            <Dialog.Close
              className="absolute top-5 right-5 flex size-8 items-center justify-center rounded-full bg-foreground/5 text-muted-foreground transition-colors hover:bg-foreground/10 hover:text-foreground"
              aria-label="Close"
            >
              <IconX className="size-4" stroke={1.8} />
            </Dialog.Close>
            {/* One slot box, so multi-node children lay out as they always did. */}
            {children ? <div className="min-w-0">{children}</div> : null}
            {footer ? <div className="flex justify-end gap-2 pt-2">{footer}</div> : null}
          </Dialog.Content>
        </Dialog.Overlay>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
