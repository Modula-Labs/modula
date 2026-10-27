import { useEffect, useState } from "react";
import { Spinner } from "@/components/ui/Button";
import { MOVED_EVENT } from "@/lib/api";
import { getMaintenance, type Maintenance } from "@/lib/migrate";

const POLL_MS = 3000;

function sameOrigin(url: string): boolean {
  try {
    return new URL(url, window.location.href).origin === window.location.origin;
  } catch {
    return false;
  }
}

/**
 * Full-screen notice while the control plane moves to a new server: the
 * dashboard is read-only ("maintenance"), then the old server answers
 * "moved". Once moved, waits for the new address and goes there. Mounted
 * once at the app root; the poll is one small public request every few seconds.
 */
export function MaintenanceOverlay() {
  const [state, setState] = useState<Maintenance>({ mode: null, message: null, newUrl: null });

  useEffect(() => {
    let stop = false;
    let timer: number | undefined;
    const tick = async () => {
      if (!document.hidden) {
        const m = await getMaintenance();
        if (stop) return;
        // Keep a seen `moved` target even if a later answer lacks it.
        setState((prev) => (m.mode === null && prev.mode === "moved" ? prev : { ...m, newUrl: m.newUrl ?? prev.newUrl }));
      }
      timer = window.setTimeout(tick, POLL_MS);
    };
    const onMoved = (e: Event) => {
      const url = (e as CustomEvent<string>).detail;
      setState({ mode: "moved", message: null, newUrl: url });
    };
    window.addEventListener(MOVED_EVENT, onMoved);
    void tick();
    return () => {
      stop = true;
      window.clearTimeout(timer);
      window.removeEventListener(MOVED_EVENT, onMoved);
    };
  }, []);

  const moved = state.mode === "moved";
  const newUrl = state.newUrl;
  useEffect(() => {
    if (!moved || !newUrl) return;
    let stop = false;
    const base = newUrl.replace(/\/+$/, "");
    const check = async (): Promise<boolean> => {
      try {
        if (sameOrigin(base)) {
          const res = await fetch("/api/health", { cache: "no-store" });
          return res.ok;
        }
        // Opaque answer: reachable, with a valid certificate.
        await fetch(`${base}/api/health`, { mode: "no-cors", cache: "no-store" });
        return true;
      } catch {
        return false;
      }
    };
    const loop = async () => {
      while (!stop) {
        if (await check()) {
          if (sameOrigin(base)) window.location.reload();
          else window.location.href = base;
          return;
        }
        await new Promise((r) => setTimeout(r, POLL_MS));
      }
    };
    void loop();
    return () => {
      stop = true;
    };
  }, [moved, newUrl]);

  if (!state.mode) return null;

  return (
    <div
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="maintenance-title"
      className="fixed inset-0 z-[100] flex items-center justify-center bg-background/95 p-6 backdrop-blur-sm"
    >
      <div className="flex max-w-md flex-col items-center gap-4 text-center">
        <Spinner className="size-6" />
        <h1 id="maintenance-title" className="text-lg font-bold">
          {moved ? "Moving to the new server…" : "Moving the dashboard…"}
        </h1>
        {moved ? (
          <>
            <p className="text-sm text-muted-foreground">
              {state.message ?? "The dashboard now runs on a new server."} You'll be taken there
              as soon as it answers. DNS caches may need a few minutes; if it sticks, flush your
              local DNS cache.
            </p>
            {newUrl ? (
              <a className="font-mono text-sm underline" href={newUrl}>
                {newUrl}
              </a>
            ) : null}
          </>
        ) : (
          <p className="text-sm text-muted-foreground">
            {state.message ??
              "The control plane is being copied to the new server. Changes are refused for a few minutes; your apps keep running."}
          </p>
        )}
      </div>
    </div>
  );
}
