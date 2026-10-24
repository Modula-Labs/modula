import { clsx } from "clsx";
import type { ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Alert, Skeleton } from "@/components/ui/misc";

/** Query options shared by the database endpoints. */
export const dbQueryOpts = {
  retry: 1,
  refetchOnWindowFocus: false,
} as const;

/** Renders loading / error for a query; `children` runs once data exists. */
export function QueryBoundary<T>({
  q,
  skeleton = <Skeleton className="h-40" />,
  children,
}: {
  q: { data: T | undefined; error: Error | null; isPending: boolean; refetch: () => unknown };
  skeleton?: ReactNode;
  children: (data: T) => ReactNode;
}) {
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
  return <Button type="button" variant="ghost" size="icon" aria-label={label} title={label} className={clsx("text-muted-foreground hover:text-foreground", className)} {...rest} />;
}

export function Card({ className, children }: { className?: string; children: ReactNode }) {
  return <div className={clsx("rounded-xl border border-border bg-card", className)}>{children}</div>;
}

export function SectionTitle({ title, hint, right }: { title: string; hint?: ReactNode; right?: ReactNode }) {
  return (
    <div className="mb-3 flex items-end justify-between gap-3">
      <div>
        <h3 className="text-base font-bold">{title}</h3>
        {hint ? <p className="mt-0.5 text-sm text-muted-foreground">{hint}</p> : null}
      </div>
      {right}
    </div>
  );
}
