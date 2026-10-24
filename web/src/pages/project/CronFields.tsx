import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Field, Input } from "@/components/ui/Input";
import { api } from "@/lib/api";
import { formatUtc, relativeUntil } from "@/lib/format";

/** The schedule's meaning as the server reads it: description and next runs, or why it's invalid. */
function CronPreview({ schedule }: { schedule: string }) {
  const [debounced, setDebounced] = useState(schedule);
  useEffect(() => {
    const t = window.setTimeout(() => setDebounced(schedule), 300);
    return () => window.clearTimeout(t);
  }, [schedule]);
  const preview = useQuery({
    queryKey: ["cron-preview", debounced],
    queryFn: () => api.cronPreview(debounced),
    placeholderData: (prev) => prev,
    retry: false,
  });
  const p = preview.data;
  if (preview.error) return <p className="text-sm text-error">{preview.error.message}</p>;
  if (!p) return <p className="text-sm text-muted-foreground">Checking…</p>;
  if (!p.valid) return <p className="text-sm text-error">{p.error ?? "Invalid schedule"}</p>;
  return (
    <div className="flex flex-col gap-1 text-sm">
      <p className="font-bold">{p.description}</p>
      <ul className="text-muted-foreground">
        {p.upcoming.map((t) => (
          <li key={t}>
            {formatUtc(t)} ({relativeUntil(t)})
          </li>
        ))}
      </ul>
    </div>
  );
}

/** Schedule and timeout inputs for a cron service, with the live preview under the schedule. */
export function CronFields({
  schedule,
  timeout,
  canEdit,
  onSchedule,
  onTimeout,
}: {
  schedule: string;
  timeout: string;
  canEdit: boolean;
  onSchedule: (v: string) => void;
  onTimeout: (v: string) => void;
}) {
  const trimmed = schedule.trim();
  return (
    <>
      <Field label="Schedule" hint="UTC. Leave empty to run continuously.">
        {(p) => (
          <Input
            {...p}
            mono
            disabled={!canEdit}
            placeholder="0 3 * * *"
            value={schedule}
            onChange={(e) => onSchedule(e.target.value)}
          />
        )}
      </Field>
      {trimmed ? <CronPreview schedule={trimmed} /> : null}
      {trimmed ? (
        <Field label="Timeout (seconds)" hint="A run still going after this is stopped. 1 to 86400, empty uses 3600.">
          {(p) => (
            <Input
              {...p}
              className="w-32"
              inputMode="numeric"
              disabled={!canEdit}
              placeholder="3600"
              value={timeout}
              onChange={(e) => onTimeout(e.target.value)}
            />
          )}
        </Field>
      ) : null}
    </>
  );
}
