import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { ChevronDown, ChevronRight, X } from "@/components/ui/icons";
import { Select } from "@/components/ui/Input";
import { Skeleton } from "@/components/ui/misc";
import { ApiError, api, type ActivityGroup, type ActivityKind, type ServiceSummary } from "@/lib/api";
import { relativeTime } from "@/lib/format";
import { describeChange, groupTitle, initials } from "./activity";

/** Floating list over the canvas: what changed in this environment, newest first. */
export function ActivityPanel({
  projectId,
  environmentId,
  services,
  onOpenDeployment,
  onClose,
}: {
  projectId: string;
  environmentId: string;
  services: ServiceSummary[];
  onOpenDeployment: (serviceId: string | null, deploymentId: string) => void;
  onClose: () => void;
}) {
  const [kind, setKind] = useState<ActivityKind | "">("");
  const [serviceId, setServiceId] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const activity = useQuery({
    queryKey: ["activity", projectId, environmentId, serviceId, kind],
    queryFn: async () => {
      try {
        return await api.activity(projectId, {
          environmentId,
          serviceId: serviceId || undefined,
          kind: kind || undefined,
        });
      } catch (e) {
        // Older servers don't have the endpoint yet.
        if (e instanceof ApiError && e.status === 404) return { items: [] as ActivityGroup[] };
        throw e;
      }
    },
    refetchInterval: 10_000,
    retry: false,
  });
  const items = activity.data?.items ?? [];
  const toggle = (id: string) =>
    setCollapsed((s) => {
      const n = new Set(s);
      if (!n.delete(id)) n.add(id);
      return n;
    });

  return (
    <aside
      aria-label="Activity"
      className="anim-sheet absolute top-14 right-3 bottom-3 z-10 flex w-[380px] max-w-[calc(100%-24px)] flex-col rounded-xl border border-border bg-popover shadow-menu"
    >
      <header className="flex items-center justify-between px-4 pt-3 pb-2">
        <h2 className="text-base font-bold">Activity</h2>
        <Button variant="ghost" size="icon-sm" onClick={onClose} aria-label="Close activity">
          <X />
        </Button>
      </header>
      <div className="flex gap-2 px-4 pb-3">
        <Select aria-label="Filter by activity" value={kind} onChange={(e) => setKind(e.target.value as ActivityKind | "")}>
          <option value="">All activity</option>
          <option value="deployment">Deployments</option>
          <option value="variable">Variables</option>
          <option value="config">Settings</option>
        </Select>
        <Select aria-label="Filter by service" value={serviceId} onChange={(e) => setServiceId(e.target.value)}>
          <option value="">All services</option>
          {services.map((s) => (
            <option key={s.id} value={s.id}>
              {s.name}
            </option>
          ))}
        </Select>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto border-t border-border">
        {activity.isPending ? (
          <div className="flex flex-col gap-3 p-4">
            <Skeleton className="h-12" />
            <Skeleton className="h-12" />
          </div>
        ) : activity.error ? (
          <p className="p-4 text-sm text-error">Couldn't load activity: {activity.error.message}</p>
        ) : items.length === 0 ? (
          <p className="p-6 text-center text-sm text-muted-foreground">No activity yet.</p>
        ) : (
          items.map((g) => {
            const open = !collapsed.has(g.id);
            return (
              <section key={g.id} className="border-b border-border px-4 py-3 last:border-b-0">
                <button onClick={() => toggle(g.id)} aria-expanded={open} className="flex w-full cursor-pointer items-center gap-2 text-left">
                  {open ? <ChevronDown className="size-4 text-muted-foreground" /> : <ChevronRight className="size-4 text-muted-foreground" />}
                  <span className="min-w-0 flex-1 truncate text-sm font-bold">{groupTitle(g)}</span>
                </button>
                <div className="mt-1 flex items-center gap-2 pl-6 text-xs text-muted-foreground">
                  <Avatar actor={g.actor} />
                  <span className="truncate">{g.actor?.name ?? "System"}</span>
                  <span>·</span>
                  <span className="whitespace-nowrap">{relativeTime(g.at)}</span>
                </div>
                {open ? (
                  <ul className="mt-2 flex flex-col gap-1 pl-6">
                    {g.changes.map((c, i) => {
                      const text = describeChange(c);
                      const id = c.deploymentId;
                      return (
                        <li key={i} className="flex items-baseline justify-between gap-3 text-sm">
                          {id ? (
                            <button onClick={() => onOpenDeployment(g.serviceId, id)} className="min-w-0 truncate text-left hover:underline">
                              {text}
                            </button>
                          ) : (
                            <span className="min-w-0 truncate">{text}</span>
                          )}
                          <span className="text-xs whitespace-nowrap text-muted-foreground">{relativeTime(c.at)}</span>
                        </li>
                      );
                    })}
                  </ul>
                ) : null}
              </section>
            );
          })
        )}
      </div>
    </aside>
  );
}

function Avatar({ actor }: { actor: ActivityGroup["actor"] }) {
  if (actor?.avatarUrl) return <img src={actor.avatarUrl} alt="" className="size-4 rounded-full" />;
  return (
    <span aria-hidden className="flex size-4 items-center justify-center rounded-full bg-muted text-[8px] font-bold text-foreground">
      {actor ? initials(actor.name) : "·"}
    </span>
  );
}
