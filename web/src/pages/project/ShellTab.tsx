import { useQuery } from "@tanstack/react-query";
import { lazy, Suspense, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Terminal } from "@/components/ui/icons";
import { Alert, EmptyState, Skeleton, StatusDot } from "@/components/ui/misc";
import { api, ensureSudo } from "@/lib/api";

// xterm.js is only needed once a shell opens.
const ShellTerminal = lazy(() => import("./ShellTerminal"));

type Phase = "idle" | "authorizing" | "connecting" | "open" | "closed";

/** A shell in the service's running container (Member+, re-authenticated;
 *  every session is audited, what is typed is not). */
export function ShellTab({ instanceId }: { instanceId: string }) {
  const list = useQuery({ queryKey: ["deployments", instanceId], queryFn: () => api.deployments(instanceId) });
  const active = list.data?.find((d) => d.status === "active" && !d.hosting);
  const [phase, setPhase] = useState<Phase>("idle");
  const [session, setSession] = useState(0);
  const [error, setError] = useState<string | null>(null);

  if (list.isPending) return <Skeleton className="h-64" />;
  if (!active) {
    return (
      <EmptyState
        icon={<Terminal />}
        title="No running deployment"
        body="Deploy the service to open a shell in its container."
      />
    );
  }

  const open = async () => {
    setError(null);
    setPhase("authorizing");
    try {
      if (!(await ensureSudo())) {
        setPhase("idle");
        return;
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setPhase("idle");
      return;
    }
    setSession((n) => n + 1);
    setPhase("connecting");
  };

  if (phase === "idle" || phase === "authorizing") {
    return (
      <div className="flex flex-col gap-4">
        {error ? <Alert>{error}</Alert> : null}
        <EmptyState
          icon={<Terminal />}
          title="Shell"
          body={
            <>
              Run commands in the running container, as <span className="font-mono">bash</span> when the image has it.
              Opening a shell is recorded in the activity log; what you type is not.
            </>
          }
          action={
            <Button variant="secondary" icon={<Terminal />} loading={phase === "authorizing"} onClick={() => void open()}>
              Open shell
            </Button>
          }
        />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-3">
        <StatusDot tone={phase === "open" ? "success" : phase === "connecting" ? "warn" : "neutral"}>
          {phase === "open" ? "Connected" : phase === "connecting" ? "Connecting…" : "Disconnected"}
        </StatusDot>
        <span className="truncate font-mono text-xs text-muted-foreground">{active.id}</span>
        <div className="ml-auto">
          {phase === "closed" ? (
            <Button size="sm" variant="secondary" onClick={() => void open()}>
              Reconnect
            </Button>
          ) : (
            <Button size="sm" variant="ghost" onClick={() => setPhase("idle")}>
              Disconnect
            </Button>
          )}
        </div>
      </div>
      <Suspense fallback={<Skeleton className="h-[min(60vh,560px)] min-h-[320px]" />}>
        <ShellTerminal
          key={session}
          deploymentId={active.id}
          onOpen={() => setPhase("open")}
          onClose={() => setPhase("closed")}
        />
      </Suspense>
    </div>
  );
}
