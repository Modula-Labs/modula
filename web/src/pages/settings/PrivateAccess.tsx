import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Field, Input, Textarea } from "@/components/ui/Input";
import { Check, Circle, X } from "@/components/ui/icons";
import { Alert, Badge, Skeleton, type Tone } from "@/components/ui/misc";
import {
  api,
  ApiError,
  type PrivateAccessPhase,
  type PrivateAccessStatus,
  type PrivateAccessStep,
} from "@/lib/api";
import {
  formatAllowList,
  isChanging,
  parseAllowList,
} from "@/lib/privateAccess";

const PHASE: Record<PrivateAccessPhase, { label: string; tone: Tone }> = {
  off: { label: "Public", tone: "neutral" },
  enabling: { label: "Turning on…", tone: "info" },
  on: { label: "Private", tone: "success" },
  disabling: { label: "Turning off…", tone: "info" },
};

function StepIcon({
  status,
}: {
  status: PrivateAccessStep["status"] | boolean;
}) {
  if (status === "done" || status === true)
    return <Check className="size-4 shrink-0 text-success" aria-label="Done" />;
  if (status === "failed" || status === false)
    return <X className="size-4 shrink-0 text-error" aria-label="Not ready" />;
  return (
    <Circle
      className="size-4 shrink-0 animate-pulse text-muted-foreground"
      aria-label="Running"
    />
  );
}

function Steps({ steps }: { steps: PrivateAccessStep[] }) {
  if (steps.length === 0) return null;
  return (
    <ol className="flex flex-col gap-1.5 text-sm">
      {steps.map((s, i) => (
        <li key={`${i}-${s.label}`} className="flex items-start gap-2">
          <StepIcon status={s.status} />
          <span>
            {s.label}
            {s.detail ? (
              <span className="block text-[13px] text-muted-foreground">
                {s.detail}
              </span>
            ) : null}
          </span>
        </li>
      ))}
    </ol>
  );
}

/** Settings → Private dashboard: Cloudflare Tunnel + Access in front of the dashboard, API and servers. */
export function PrivateAccessSection({ workspaceId }: { workspaceId: string }) {
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["private-access"],
    queryFn: api.privateAccess,
    refetchInterval: (query) =>
      isChanging(query.state.data?.phase) ? 2000 : false,
  });
  const phase = q.data?.phase;
  const check = useQuery({
    queryKey: ["private-access-check", workspaceId],
    queryFn: () => api.privateAccessCheck(workspaceId),
    enabled: phase === "off",
    staleTime: 30_000,
  });
  const [allow, setAllow] = useState("");
  const [nodeHost, setNodeHost] = useState("");
  const [touched, setTouched] = useState(false);
  useEffect(() => {
    if (touched) return;
    const s = q.data;
    if (s && (s.allowedEmails.length || s.allowedDomains.length))
      setAllow(formatAllowList(s.allowedEmails, s.allowedDomains));
    else if (check.data)
      setAllow(formatAllowList(check.data.suggestedEmails, []));
  }, [q.data, check.data, touched]);

  const set = (s: PrivateAccessStatus) =>
    qc.setQueryData(["private-access"], s);
  const parsed = parseAllowList(allow);
  const enable = useMutation({
    mutationFn: () =>
      api.setPrivateAccess({
        enabled: true,
        workspaceId,
        nodeHost: nodeHost.trim() || undefined,
        allowedEmails: parsed.emails,
        allowedDomains: parsed.domains,
      }),
    onSuccess: set,
  });
  const disable = useMutation({
    mutationFn: () => api.setPrivateAccess({ enabled: false }),
    onSuccess: set,
  });

  if (q.error instanceof ApiError && q.error.status === 404) return null;
  if (q.isPending) return <Skeleton className="h-40" />;
  if (q.error) return <Alert>{q.error.message}</Alert>;

  const s = q.data;
  const error = enable.error ?? disable.error;
  const busy = isChanging(s.phase);
  const listError = parsed.invalid.length
    ? `Not an email or domain: ${parsed.invalid.join(", ")}`
    : null;
  const canEnable =
    !!check.data?.ready &&
    !listError &&
    parsed.emails.length + parsed.domains.length > 0;

  return (
    <div className="flex flex-col gap-5">
      <div className="text-sm text-muted-foreground">
        Hides the dashboard, the API, the CLI endpoint and the servers'
        connection behind a Cloudflare Tunnel and Cloudflare Access. Only the
        people below can sign in (at Cloudflare, with an email code or your
        identity provider); the CLI and CI use a Cloudflare service token. Your
        apps stay public on their own domains.
      </div>

      <div className="flex flex-wrap items-center gap-2 text-sm">
        <Badge tone={PHASE[s.phase].tone}>{PHASE[s.phase].label}</Badge>
        {s.dashboardHost ? (
          <span className="font-mono">{s.dashboardHost}</span>
        ) : null}
        {s.phase === "on" && s.teamDomain ? (
          <span className="text-muted-foreground">· {s.teamDomain}</span>
        ) : null}
      </div>

      {error ? <Alert>{error.message}</Alert> : null}
      {s.error && !busy ? (
        <Alert
          tone={s.phase === "on" ? "warn" : "danger"}
          title={
            s.phase === "off" ? "The last change didn't go through" : undefined
          }
        >
          {s.error}
        </Alert>
      ) : null}

      {busy || s.phase === "on" ? <Steps steps={s.steps} /> : null}

      {s.phase === "off" ? (
        <>
          <div className="flex flex-col gap-2">
            <span className="text-sm font-bold">Before you start</span>
            {check.isPending ? (
              <Skeleton className="h-24" />
            ) : check.error ? (
              <Alert>{check.error.message}</Alert>
            ) : (
              <ul className="flex flex-col gap-1.5 text-sm">
                {check.data.items.map((i) => (
                  <li key={i.label} className="flex items-start gap-2">
                    <StepIcon status={i.ok} />
                    <span>
                      {i.label}
                      {i.detail ? (
                        <span className="block text-[13px] text-muted-foreground">
                          {i.detail}
                        </span>
                      ) : null}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </div>
          <Field
            label="Who may sign in"
            hint="One email address or @domain per line. Defaults to this workspace's admins."
            error={listError}
          >
            {(p) => (
              <Textarea
                {...p}
                rows={4}
                className="font-mono text-[13px]"
                value={allow}
                onChange={(e) => {
                  setTouched(true);
                  setAllow(e.target.value);
                }}
              />
            )}
          </Field>
          <Field
            label="Server hostname"
            hint="Where your servers reach this one through the tunnel. Leave empty for a name next to the dashboard's."
          >
            {(p) => (
              <Input
                {...p}
                placeholder="deploy-nodes.example.com"
                value={nodeHost}
                onChange={(e) => setNodeHost(e.target.value)}
              />
            )}
          </Field>
          <div>
            <Button
              variant="primary"
              loading={enable.isPending}
              disabled={!canEnable}
              onClick={() => enable.mutate()}
            >
              Make the dashboard private
            </Button>
          </div>
        </>
      ) : null}

      {s.phase === "on" ? (
        <div className="flex flex-col gap-3 border-t border-border pt-4 text-sm">
          <div>
            <span className="font-bold">Signed-in access: </span>
            <span className="text-muted-foreground">
              {[
                ...s.allowedEmails,
                ...s.allowedDomains.map((d) => `@${d}`),
              ].join(", ")}
            </span>
          </div>
          {s.nodeHost ? (
            <div>
              <span className="font-bold">Servers connect through: </span>
              <span className="font-mono">{s.nodeHost}</span>
            </div>
          ) : null}
          <div>
            <Button
              variant="danger-outline"
              loading={disable.isPending}
              onClick={() => disable.mutate()}
            >
              Make the dashboard public again
            </Button>
          </div>
        </div>
      ) : null}

      <div className="flex flex-col gap-1.5 border-t border-border pt-4 text-sm">
        <span className="font-bold">If Cloudflare is unreachable</span>
        <span className="text-muted-foreground">
          Open the dashboard over SSH, bypassing Cloudflare:
        </span>
        <div className="flex items-center gap-2">
          <code className="font-mono text-[13px]">{s.breakGlass}</code>
          <CopyButton value={s.breakGlass} label="Copy command" />
        </div>
      </div>
    </div>
  );
}
