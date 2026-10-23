import { clsx } from "clsx";
import type { ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Alert, Skeleton } from "@/components/ui/misc";
import { isNotAvailable } from "@/lib/api";

/** Query options shared by the database endpoints: don't hammer a server that lacks them. */
export const dbQueryOpts = {
  retry: (n: number, e: Error) => !isNotAvailable(e) && n < 1,
  refetchOnWindowFocus: false,
} as const;

export function NotAvailable() {
  return (
    <div className="rounded-xl border border-dashed border-border px-6 py-10 text-center text-sm text-muted-foreground">
      Not available on this server yet.
    </div>
  );
}

/** Renders loading / error / not-available for a query; `children` runs once data exists. */
export function QueryBoundary<T>({
  q,
  skeleton = <Skeleton className="h-40" />,
  children,
}: {
  q: { data: T | undefined; error: Error | null; isPending: boolean; refetch: () => unknown };
  skeleton?: ReactNode;
  children: (data: T) => ReactNode;
}) {
  if (q.error && isNotAvailable(q.error)) return <NotAvailable />;
  if (q.data !== undefined) return <>{children(q.data)}</>;
  if (q.error)
    return (
      <div className="flex flex-col items-start gap-2">
        <Alert>{q.error.message}</Alert>
        <Button size="sm" variant="secondary" onClick={() => void q.refetch()}>
          Retry
        </Button>
      </div>
    );
  return <>{skeleton}</>;
}

export function IconButton({
  label,
  className,
  ...rest
}: { label: string } & React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      {...rest}
      className={clsx(
        "inline-flex size-8 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground disabled:opacity-50 [&_svg]:size-4",
        className,
      )}
    />
  );
}

export function Card({ className, children }: { className?: string; children: ReactNode }) {
  return <div className={clsx("rounded-lg border border-border bg-card", className)}>{children}</div>;
}

export function SectionTitle({ title, hint, right }: { title: string; hint?: ReactNode; right?: ReactNode }) {
  return (
    <div className="mb-3 flex items-end justify-between gap-3">
      <div>
        <h3 className="text-base font-semibold">{title}</h3>
        {hint ? <p className="mt-0.5 text-sm text-muted-foreground">{hint}</p> : null}
      </div>
      {right}
    </div>
  );
}
