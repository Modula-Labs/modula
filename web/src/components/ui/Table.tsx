import { clsx } from "clsx";
import type { ComponentPropsWithoutRef } from "react";

/* Kobra Table: a radius xl container with a 1px border. Header rows sit on the muted
   fill with 36px cells; body cells are 40px with 16px sides; every cell but the last
   draws a right rule. Hover tints rows with muted at 50%, selected rows (data-selected
   or aria-selected) take the full muted fill; the footer sits on a muted wash behind a
   top border. All text is 14/20 regular. */

export function Table({ className, ...p }: ComponentPropsWithoutRef<"table">) {
  return (
    <div className="w-full overflow-x-auto rounded-xl border border-border">
      <table className={clsx("w-full caption-bottom border-collapse text-sm", className)} {...p} />
    </div>
  );
}

export function TableHeader({ className, ...p }: ComponentPropsWithoutRef<"thead">) {
  return <thead className={clsx("bg-muted [&_tr]:hover:bg-transparent", className)} {...p} />;
}

export function TableBody({ className, ...p }: ComponentPropsWithoutRef<"tbody">) {
  return <tbody className={clsx("[&_tr:last-child]:border-b-0", className)} {...p} />;
}

export function TableFooter({ className, ...p }: ComponentPropsWithoutRef<"tfoot">) {
  return <tfoot className={clsx("border-t border-border bg-muted/50 [&_tr]:border-b-0", className)} {...p} />;
}

export function TableRow({ className, ...p }: ComponentPropsWithoutRef<"tr">) {
  return (
    <tr
      className={clsx(
        "border-b border-border transition-colors hover:bg-muted/50 data-[selected=true]:bg-muted aria-selected:bg-muted",
        className,
      )}
      {...p}
    />
  );
}

export function TableHead({ className, ...p }: ComponentPropsWithoutRef<"th">) {
  return (
    <th
      className={clsx(
        "h-9 border-r border-border px-4 text-left align-middle font-normal whitespace-nowrap text-foreground last:border-r-0",
        className,
      )}
      {...p}
    />
  );
}

export function TableCell({ className, ...p }: ComponentPropsWithoutRef<"td">) {
  return <td className={clsx("h-10 border-r border-border px-4 align-middle last:border-r-0", className)} {...p} />;
}

export function TableCaption({ className, ...p }: ComponentPropsWithoutRef<"caption">) {
  return <caption className={clsx("mt-4 text-sm text-muted-foreground", className)} {...p} />;
}
