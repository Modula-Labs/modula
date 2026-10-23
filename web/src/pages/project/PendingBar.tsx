import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useEffect, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { Button } from "@/components/ui/Button";
import { Check, ChevronDown, ChevronRight, Dots, Minus, Pencil, Plus, Trash, X } from "@/components/ui/icons";
import { Input } from "@/components/ui/Input";
import { Menu, MenuItem } from "@/components/ui/Menu";
import { Modal } from "@/components/ui/Modal";
import { Alert, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type PendingChange, type PendingChanges, type PendingService } from "@/lib/api";
import { useMe } from "@/lib/workspace";
import { initials, words } from "./activity";

const none: PendingChanges = { count: 0, services: [] };

/** Edits not yet deployed in this environment. Shared with the page so it can
 *  demote its own primary button and badge edited cards while the bar is up. */
export function usePending(environmentId: string | undefined) {
  return useQuery({
    queryKey: ["pending", environmentId],
    enabled: !!environmentId,
    queryFn: async () => {
      try {
        return await api.pending(environmentId!);
      } catch (e) {
        // Older servers don't track pending changes.
        if (e instanceof ApiError && e.status === 404) return none;
        throw e;
      }
    },
    refetchInterval: 10_000,
    retry: false,
  });
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** Fields, editable elements, buttons and links keep Shift+Enter for themselves. */
function isTyping(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  return !!el && (el.isContentEditable || ["INPUT", "TEXTAREA", "SELECT", "BUTTON", "A"].includes(el.tagName));
}

/** Railway-style "Apply N changes" pill at the top of the canvas, with the
 *  details modal (per-change diff, discard, deploy message). */
export function PendingBar({
  projectId,
  environmentId,
  canDiscard,
}: {
  projectId: string;
  environmentId: string;
  /** Members and up may discard (it rewrites variables and settings). */
  canDiscard: boolean;
}) {
  const qc = useQueryClient();
  const pending = usePending(environmentId).data ?? none;
  const [details, setDetails] = useState(false);
  const [message, setMessage] = useState("");
  const [done, setDone] = useState<number | null>(null);
  useEffect(() => {
    if (done === null) return;
    const t = window.setTimeout(() => setDone(null), 4000);
    return () => window.clearTimeout(t);
  }, [done]);

  const refresh = () =>
    Promise.all([
      qc.invalidateQueries({ queryKey: ["pending", environmentId] }),
      qc.invalidateQueries({ queryKey: ["pending-values", environmentId] }),
      qc.invalidateQueries({ queryKey: ["project", projectId] }),
      qc.invalidateQueries({ queryKey: ["deployments"] }),
      qc.invalidateQueries({ queryKey: ["activity"] }),
    ]);

  const deploy = useMutation({
    mutationFn: () => api.deployPending(environmentId, { message: message.trim() || undefined }),
    onSuccess: async (r) => {
      setDone(r.deployments.filter((d) => d.deploymentId).length);
      setDetails(false);
      setMessage("");
      await refresh();
    },
  });
  const discard = useMutation({
    mutationFn: (v: { serviceId: string; changes?: { kind: "variable" | "config"; key: string }[] }) =>
      api.discardPending(environmentId, v.serviceId, v.changes),
    onSuccess: async (r) => {
      if (r.count === 0) setDetails(false);
      await refresh();
    },
  });

  const n = pending.count;
  // Shift+Enter deploys from anywhere on the canvas (not while typing, and not
  // under another dialog; the details modal handles its own).
  useEffect(() => {
    if (n === 0) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Enter" || !e.shiftKey || e.repeat || isTyping(e.target)) return;
      if (document.querySelector('[role="dialog"]') || deploy.isPending) return;
      e.preventDefault();
      deploy.mutate();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [n, deploy]);

  if (n === 0 && done === null && !deploy.error) return null;
  return (
    <>
      <div className="anim-pop absolute top-3 left-1/2 z-10 flex max-w-[calc(100%-24px)] -translate-x-1/2 flex-col items-center gap-2">
        <div role="region" aria-label="Pending changes" className="flex items-center gap-1 rounded-xl border border-border bg-card p-1 pl-3 shadow-modal">
          {n > 0 ? (
            <>
              <span className="mr-1 text-sm font-medium whitespace-nowrap">Apply {plural(n, "change")}</span>
              <Button size="sm" variant="ghost" onClick={() => setDetails(true)}>
                Details
              </Button>
              <Button size="sm" variant="primary" loading={deploy.isPending} onClick={() => deploy.mutate()} title="Deploy (Shift+Enter)">
                Deploy
                <kbd className="ml-0.5 rounded bg-black/15 px-1 font-sans text-[11px] leading-4 font-medium">⇧+Enter</kbd>
              </Button>
              {canDiscard ? (
                <Menu
                  trigger={
                    <button aria-label="More actions" className="flex size-8 items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground">
                      <Dots className="size-4" />
                    </button>
                  }
                >
                  <MenuItem
                    danger
                    icon={<Trash />}
                    disabled={discard.isPending}
                    onSelect={() => {
                      void (async () => {
                        for (const s of pending.services) {
                          if (s.changes.some((c) => c.discardable)) await discard.mutateAsync({ serviceId: s.serviceId });
                        }
                      })().catch(() => {});
                    }}
                  >
                    Discard all changes
                  </MenuItem>
                </Menu>
              ) : null}
            </>
          ) : (
            <span className="flex h-8 items-center gap-2 pr-2 text-sm">
              <Check className="size-4 text-success" />
              {done === null ? "Nothing to deploy" : `Deploying ${plural(done, "service")}`}
            </span>
          )}
        </div>
        {(deploy.error || discard.error) && !details ? (
          <div className="w-[420px] max-w-full">
            <Alert>{(deploy.error ?? discard.error)!.message}</Alert>
          </div>
        ) : null}
      </div>
      <DetailsModal
        open={details && n > 0}
        onOpenChange={setDetails}
        environmentId={environmentId}
        pending={pending}
        canDiscard={canDiscard}
        message={message}
        onMessage={setMessage}
        deploying={deploy.isPending}
        onDeploy={() => deploy.mutate()}
        discarding={discard.isPending}
        onDiscard={(serviceId, changes) => discard.mutate({ serviceId, changes })}
        error={(deploy.error ?? discard.error)?.message}
      />
    </>
  );
}

function DetailsModal({
  open,
  onOpenChange,
  environmentId,
  pending,
  canDiscard,
  message,
  onMessage,
  deploying,
  onDeploy,
  discarding,
  onDiscard,
  error,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  environmentId: string;
  pending: PendingChanges;
  canDiscard: boolean;
  message: string;
  onMessage: (m: string) => void;
  deploying: boolean;
  onDeploy: () => void;
  discarding: boolean;
  onDiscard: (serviceId: string, changes?: { kind: "variable" | "config"; key: string }[]) => void;
  error?: string;
}) {
  const me = useMe().data;
  // Values are fetched only while the modal is open (Members see them; others get names).
  const values = useQuery({
    queryKey: ["pending-values", environmentId],
    queryFn: () => api.pending(environmentId, { values: true }),
    enabled: open,
    staleTime: 0,
  });
  const services = values.data?.services ?? pending.services;
  const n = values.data?.count ?? pending.count;
  const names = services.map((s) => s.serviceName);

  return (
    <Modal open={open} onOpenChange={onOpenChange} title={`${plural(n, "change")} to apply`} width={760}>
      <div
        className="flex flex-col gap-4"
        onKeyDown={(e: ReactKeyboardEvent) => {
          if (e.key === "Enter" && e.shiftKey && !deploying) {
            e.preventDefault();
            onDeploy();
          }
        }}
      >
        <label className="flex items-center gap-3 rounded-lg border border-border px-3 py-2 focus-within:border-ring">
          <span aria-hidden className="flex size-6 shrink-0 items-center justify-center rounded-full bg-muted text-[10px] font-semibold">
            {me ? initials(me.user.name) : <Pencil className="size-3.5" />}
          </span>
          <span className="sr-only">Deploy message</span>
          <Input
            value={message}
            onChange={(e) => onMessage(e.target.value)}
            maxLength={500}
            placeholder="Describe these changes (optional)"
            className="h-7 border-0 px-0 focus:border-0"
          />
        </label>
        {error ? <Alert>{error}</Alert> : null}
        <div className="flex max-h-[min(60vh,560px)] flex-col gap-3 overflow-y-auto">
          {values.isPending && open ? <Skeleton className="h-24" /> : null}
          {services.map((s) => (
            <ServiceChanges key={s.serviceId} service={s} canDiscard={canDiscard} discarding={discarding} onDiscard={onDiscard} />
          ))}
        </div>
        <div className="flex items-center justify-between gap-3 border-t border-border pt-4">
          <span className="min-w-0 truncate text-sm text-muted-foreground">
            {names.length ? `${names.join(", ")} will redeploy` : null}
          </span>
          <Button variant="primary" loading={deploying} disabled={n === 0} onClick={onDeploy}>
            Deploy Changes
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function ServiceChanges({
  service,
  canDiscard,
  discarding,
  onDiscard,
}: {
  service: PendingService;
  canDiscard: boolean;
  discarding: boolean;
  onDiscard: (serviceId: string, changes?: { kind: "variable" | "config"; key: string }[]) => void;
}) {
  const [open, setOpen] = useState(true);
  const vars = service.changes.filter((c) => c.kind === "variable").length;
  const settings = service.changes.length - vars;
  const summary = [vars ? plural(vars, "Variable") : null, settings ? plural(settings, "Setting") : null].filter(Boolean).join(" · ");
  const discardable = canDiscard && service.changes.some((c) => c.discardable);
  return (
    <section aria-label={service.serviceName} className="rounded-lg border border-border">
      <header className="flex items-center gap-2 px-3 py-2.5">
        <button onClick={() => setOpen((o) => !o)} aria-expanded={open} className="flex min-w-0 flex-1 items-center gap-2 text-left">
          {open ? <ChevronDown className="size-4 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-4 shrink-0 text-muted-foreground" />}
          <span className="min-w-0 truncate text-sm">
            <span className="font-semibold">{service.serviceName}</span> will be updated
          </span>
          <span className="shrink-0 text-xs text-muted-foreground">{summary}</span>
        </button>
        {discardable ? (
          <Button size="sm" variant="ghost" disabled={discarding} onClick={() => onDiscard(service.serviceId)}>
            Discard
          </Button>
        ) : null}
      </header>
      {open ? (
        <table className="w-full table-fixed border-t border-border text-sm">
          <thead>
            <tr className="text-left text-xs text-muted-foreground">
              <th className="w-[34%] py-2 pl-3 font-medium">Change</th>
              <th className="py-2 pl-3 font-medium">Current Value</th>
              <th className="py-2 pl-3 font-medium">New Value</th>
              <th className="w-10" aria-label="Discard" />
            </tr>
          </thead>
          <tbody>
            {service.changes.map((c) => (
              <ChangeRow
                key={`${c.kind}:${c.shared ? "shared." : ""}${c.key}`}
                change={c}
                canDiscard={canDiscard && !!c.discardable && !discarding}
                onDiscard={() => c.key && (c.kind === "variable" || c.kind === "config") && onDiscard(service.serviceId, [{ kind: c.kind, key: c.key }])}
              />
            ))}
          </tbody>
        </table>
      ) : null}
    </section>
  );
}

const opIcon = {
  added: { icon: <Plus />, label: "Added", tone: "text-success" },
  removed: { icon: <Minus />, label: "Removed", tone: "text-error" },
  updated: { icon: <Pencil />, label: "Changed", tone: "text-warning" },
} as const;

function ChangeRow({ change: c, canDiscard, onDiscard }: { change: PendingChange; canDiscard: boolean; onDiscard: () => void }) {
  const op = opIcon[c.op as keyof typeof opIcon] ?? opIcon.updated;
  const name = c.kind === "variable" ? (c.shared ? `shared.${c.key}` : c.key) : c.kind === "config" ? words(c.key ?? "settings") : `service ${c.op}`;
  return (
    <tr className="border-t border-border/60 align-top">
      <td className="py-2 pl-3">
        <span className="flex min-w-0 items-center gap-2">
          <span aria-label={op.label} title={op.label} className={clsx("shrink-0 [&_svg]:size-3.5", op.tone)}>
            {op.icon}
          </span>
          <span className={clsx("truncate", c.kind === "variable" && "font-mono text-[13px]")} title={name ?? undefined}>
            {name}
          </span>
          {c.kind === "config" ? <span className="shrink-0 text-xs text-muted-foreground">Setting</span> : null}
        </span>
      </td>
      <td className="py-2 pl-3">
        <Value change={c} value={c.currentValue} absent={c.op === "added"} />
      </td>
      <td className="py-2 pl-3">
        <Value change={c} value={c.newValue} absent={c.op === "removed"} />
      </td>
      <td className="py-1.5 pr-2 text-right">
        {canDiscard ? (
          <button
            onClick={onDiscard}
            aria-label={`Discard change to ${c.key}`}
            title="Discard this change"
            className="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
          >
            <X className="size-3.5" />
          </button>
        ) : null}
      </td>
    </tr>
  );
}

function Value({ change, value, absent }: { change: PendingChange; value?: string | null; absent: boolean }) {
  if (absent) return <span className="text-muted-foreground">—</span>;
  if (change.sealed) return <span className="font-mono tracking-widest text-muted-foreground" title="Sealed: never shown">••••••</span>;
  if (value === undefined || value === null) return <span className="text-muted-foreground">Hidden</span>;
  return (
    <span className="block truncate font-mono text-[13px]" title={value}>
      {value === "" ? <span className="text-muted-foreground">(empty)</span> : value}
    </span>
  );
}
