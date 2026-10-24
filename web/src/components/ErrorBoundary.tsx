import { Component, useEffect, type ErrorInfo, type ReactNode } from "react";
import { X } from "@/components/ui/icons";
import { dismissToast, toastError, useToasts } from "@/lib/toast";

/** Keeps a crash in one part of the page from blanking the whole app: the
 *  error becomes a toast and the part re-renders. If it crashes again right
 *  away, a small retry note stands in for it instead of looping. */
export class ErrorBoundary extends Component<{ children: ReactNode }, { error: Error | null; key: number }> {
  state = { error: null as Error | null, key: 0 };
  private lastReset = 0;

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error(error, info.componentStack);
    toastError(error);
    if (Date.now() - this.lastReset > 2000) this.reset();
  }

  reset = () => {
    this.lastReset = Date.now();
    this.setState((s) => ({ error: null, key: s.key + 1 }));
  };

  render() {
    if (this.state.error) return <ErrorNote onRetry={this.reset} />;
    return <ErrorKey key={this.state.key}>{this.props.children}</ErrorKey>;
  }
}

const ErrorKey = ({ children }: { children: ReactNode }) => <>{children}</>;

function ErrorNote({ onRetry }: { onRetry: () => void }) {
  return (
    <div className="m-6 flex items-center justify-between gap-3 rounded-lg border border-border px-4 py-3 text-sm text-muted-foreground">
      <span>This part of the page couldn't be shown.</span>
      <button type="button" onClick={onRetry} className="font-medium text-foreground underline">
        Retry
      </button>
    </div>
  );
}

/** The router's fallback for a route that throws: a toast, and a retry note in
 *  place of that route only (the shell around it stays). */
export function RouteError({ error, reset }: { error: unknown; reset: () => void }) {
  useEffect(() => {
    toastError(error);
  }, [error]);
  return <ErrorNote onRetry={reset} />;
}

export function Toaster() {
  const toasts = useToasts();
  useEffect(() => {
    const onRejection = (e: PromiseRejectionEvent) => {
      // Requests already show their errors in place; this catches the rest.
      if ((e.reason as { status?: number } | null)?.status === 401) return;
      toastError(e.reason);
    };
    window.addEventListener("unhandledrejection", onRejection);
    return () => window.removeEventListener("unhandledrejection", onRejection);
  }, []);
  if (toasts.length === 0) return null;
  return (
    <div aria-live="polite" className="pointer-events-none fixed right-4 bottom-4 z-[100] flex w-[min(380px,calc(100%-32px))] flex-col gap-2">
      {toasts.map((t) => (
        <div
          key={t.id}
          role={t.tone === "error" ? "alert" : "status"}
          className="anim-pop pointer-events-auto flex items-start gap-2 rounded-xl bg-popover px-3.5 py-3 text-sm text-popover-foreground shadow-menu ring-1 ring-foreground/10"
        >
          {t.tone === "error" ? <span aria-hidden className="mt-1.5 size-1.5 shrink-0 rounded-full bg-error" /> : null}
          <span className="min-w-0 flex-1 break-words">{t.message}</span>
          <button type="button" aria-label="Dismiss" onClick={() => dismissToast(t.id)} className="-m-1 rounded-md p-1 text-muted-foreground hover:text-foreground">
            <X className="size-3.5" />
          </button>
        </div>
      ))}
    </div>
  );
}
