import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { Button } from "@/components/ui/Button";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { alertsApi, describeRule, ruleScope } from "@/lib/alerts";
import { SettingsSection } from "./SettingsParts";

/** Alert rules that apply to this project; they are created and edited in workspace settings. */
export function AlertsSection({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const rules = useQuery({ queryKey: ["project-alert-rules", projectId], queryFn: () => alertsApi.projectRules(projectId), retry: false });
  return (
    <SettingsSection title="Alerts" description="Rules that watch this project, including those for the whole workspace. Channels and rules are managed in workspace settings.">
      <div className="flex max-w-2xl flex-col gap-4">
        {rules.error ? <Alert tone="neutral">{rules.error.message}</Alert> : null}
        {rules.isLoading ? (
          <Skeleton className="h-16" />
        ) : rules.data && rules.data.length > 0 ? (
          <ul className="flex flex-col gap-2">
            {rules.data.map((r) => (
              <li key={r.id} className="flex flex-col gap-1 rounded-lg border border-border p-3">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-bold">{r.name}</span>
                  {r.firing > 0 ? <Badge tone="danger">{r.firing} firing</Badge> : null}
                  {r.enabled ? null : <Badge>Disabled</Badge>}
                </div>
                <div className="text-sm">{describeRule(r)}</div>
                <div className="text-[13px] text-muted-foreground">{ruleScope(r)}</div>
              </li>
            ))}
          </ul>
        ) : rules.data ? (
          <p className="rounded-xl border border-dashed border-border px-4 py-6 text-center text-sm text-muted-foreground">No alert rules apply to this project.</p>
        ) : null}
        <div>
          <Button onClick={() => void navigate({ to: "/settings" })}>Manage in workspace settings</Button>
        </div>
      </div>
    </SettingsSection>
  );
}
