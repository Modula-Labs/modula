import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Sheet } from "@/components/ui/Sheet";
import { TabPanel, Tabs } from "@/components/ui/Tabs";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { api } from "@/lib/api";
import { relativeTime } from "@/lib/format";
import { deploymentBadge, isLive } from "@/lib/status";
import { LogViewer } from "./LogViewer";

type Tab = "details" | "logs";

export function DeploymentSheet({ deploymentId, onClose }: { deploymentId: string; onClose: () => void }) {
  const qc = useQueryClient();
  const [tab, setTab] = useState<Tab>("logs");
  const dep = useQuery({
    queryKey: ["deployment", deploymentId],
    queryFn: () => api.deployment(deploymentId),
    refetchInterval: (q) => (q.state.data && isLive(q.state.data.status) ? 2000 : 15_000),
  });
  const remove = useMutation({
    mutationFn: () => api.removeDeployment(deploymentId),
    onSuccess: () => qc.invalidateQueries(),
  });
  const d = dep.data;

  return (
    <Sheet
      stacked
      onClose={onClose}
      header={
        d ? (
          <div className="flex flex-col gap-1.5">
            <div className="flex items-center gap-2.5">
              <Badge tone={deploymentBadge[d.status].tone}>{deploymentBadge[d.status].label}</Badge>
              <span className="truncate font-mono text-[13px]">{d.image}</span>
            </div>
            <div className="text-sm text-muted-foreground">
              {d.trigger} · {relativeTime(d.createdAt)}
            </div>
          </div>
        ) : (
          <Skeleton className="h-10 w-72" />
        )
      }
    >
      <Tabs
        value={tab}
        onChange={setTab}
        tabs={[
          { value: "details", label: "Details" },
          { value: "logs", label: "Deploy logs" },
        ]}
      >
        <TabPanel value="details">
          {d ? (
            <div className="flex flex-col gap-5">
              {d.error ? <Alert title="Deployment failed">{d.error}</Alert> : null}
              <dl className="grid grid-cols-[6rem_1fr] items-center gap-x-4 gap-y-2 text-sm">
                <dt className="text-muted-foreground">Deployment</dt>
                <dd className="flex min-w-0 items-center gap-1">
                  <span className="truncate font-mono text-[13px]">{d.id}</span>
                  <CopyButton value={d.id} label="Copy deployment id" />
                </dd>
                <dt className="text-muted-foreground">Image</dt>
                <dd className="truncate font-mono text-[13px]">{d.image}</dd>
                <dt className="text-muted-foreground">Created</dt>
                <dd>{relativeTime(d.createdAt)}</dd>
                {d.finishedAt ? (
                  <>
                    <dt className="text-muted-foreground">Finished</dt>
                    <dd>{relativeTime(d.finishedAt)}</dd>
                  </>
                ) : null}
                {d.nodeId ? (
                  <>
                    <dt className="text-muted-foreground">Node</dt>
                    <dd className="truncate font-mono text-[13px]">{d.nodeId.slice(0, 8)}</dd>
                  </>
                ) : null}
              </dl>
              <ol className="flex flex-col gap-2">
                {d.events.map((e, i) => (
                  <li key={i} className="flex gap-3 text-sm">
                    <span className="w-20 shrink-0 text-muted-foreground tabular-nums">
                      {new Date(e.at).toLocaleTimeString([], { hour12: false })}
                    </span>
                    <span className="w-24 shrink-0 font-medium">{e.phase}</span>
                    <span className="min-w-0 break-words text-muted-foreground">{e.message}</span>
                  </li>
                ))}
              </ol>
              {d.status === "active" || d.status === "crashed" || isLive(d.status) ? (
                <div>
                  {remove.error ? <Alert>{remove.error.message}</Alert> : null}
                  <Button variant="danger-outline" loading={remove.isPending} onClick={() => remove.mutate()}>
                    {isLive(d.status) ? "Cancel deployment" : "Remove deployment"}
                  </Button>
                </div>
              ) : null}
            </div>
          ) : null}
        </TabPanel>
        <TabPanel value="logs">
          <div className="flex h-full flex-col">
            <LogViewer url={api.deploymentLogsUrl(deploymentId)} downloadName={`deployment-${deploymentId}.log`} />
          </div>
        </TabPanel>
      </Tabs>
    </Sheet>
  );
}
