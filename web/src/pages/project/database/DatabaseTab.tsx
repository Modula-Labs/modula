import { useQuery } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useState } from "react";
import { Check, Circle, Database, Power } from "@/components/ui/icons";
import { Skeleton } from "@/components/ui/misc";
import { api, type Instance } from "@/lib/api";
import { ConfigView } from "./ConfigView";
import { DataView } from "./DataView";
import { StatsView } from "./StatsView";
import { Card } from "./shared";

type Sub = "data" | "stats" | "config";
const SUBS: { value: Sub; label: string }[] = [
  { value: "data", label: "Data" },
  { value: "stats", label: "Stats" },
  { value: "config", label: "Config" },
];

/** Required for the database interface to work (set when the template is created). */
const REQUIRED_VARS = ["DATABASE_URL"];

export function DatabaseTab({ instance, canEdit, onConnect }: { instance: Instance; canEdit: boolean; onConnect: () => void }) {
  const [sub, setSub] = useState<Sub>("data");
  const vars = useQuery({ queryKey: ["variables", instance.id], queryFn: () => api.variables(instance.id) });
  const online = instance.status === "online";
  const missing = vars.data ? REQUIRED_VARS.filter((n) => !vars.data.some((v) => v.name === n)) : null;
  const ready = online && missing !== null && missing.length === 0;

  return (
    <div className="flex flex-col gap-5">
      <div className="flex items-center justify-between rounded-lg bg-muted px-1.5 py-1">
        <div role="tablist" aria-label="Database" className="flex gap-0.5">
          {SUBS.map((s) => (
            <button
              key={s.value}
              role="tab"
              aria-selected={sub === s.value}
              onClick={() => setSub(s.value)}
              className={clsx(
                "h-7 rounded-md px-3 text-[13px] font-medium",
                sub === s.value ? "bg-card text-foreground shadow-raise" : "text-muted-foreground hover:text-foreground",
              )}
            >
              {s.label}
            </button>
          ))}
        </div>
        <button type="button" onClick={onConnect} className="inline-flex h-7 items-center gap-1.5 rounded-md px-2.5 text-[13px] font-medium text-muted-foreground hover:text-foreground [&_svg]:size-3.5">
          <Power /> Connect
        </button>
      </div>
      {vars.isPending && !ready ? (
        <Skeleton className="h-48" />
      ) : !ready ? (
        <Checklist online={online} missing={missing ?? REQUIRED_VARS} />
      ) : sub === "data" ? (
        <DataView instance={instance} canEdit={canEdit} />
      ) : sub === "stats" ? (
        <StatsView instance={instance} canEdit={canEdit} />
      ) : (
        <ConfigView instance={instance} canEdit={canEdit} />
      )}
    </div>
  );
}

function Checklist({ online, missing }: { online: boolean; missing: string[] }) {
  const items = [
    { label: "Deployment Online", ok: online, hint: online ? null : "Deploy the service and wait for it to come online." },
    { label: "Required Variables", ok: missing.length === 0, hint: missing.length ? `Missing ${missing.join(", ")}.` : null },
  ];
  return (
    <Card className="mx-auto flex w-full max-w-md flex-col gap-4 p-6">
      <div className="flex items-start gap-3">
        <div className="flex size-9 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground">
          <Database className="size-5" />
        </div>
        <div>
          <div className="text-base font-semibold">Postgres DB</div>
          <p className="text-sm text-muted-foreground">A custom interface into your database</p>
        </div>
      </div>
      <ul className="flex flex-col gap-2.5">
        {items.map((i) => (
          <li key={i.label} className="flex items-start gap-2.5 text-sm">
            {i.ok ? <Check className="mt-0.5 size-4 text-success" /> : <Circle className="mt-0.5 size-4 text-muted-foreground" />}
            <div>
              <div className={i.ok ? "text-foreground" : "font-medium"}>{i.label}</div>
              {i.hint ? <div className="text-xs text-muted-foreground">{i.hint}</div> : null}
            </div>
          </li>
        ))}
      </ul>
    </Card>
  );
}
