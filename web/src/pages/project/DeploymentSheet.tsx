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

type Tab = "details" | "build" | "logs";

const shortDigest = (d: string) => (d.startsWith("sha256:") ? `sha256:${d.slice(7, 19)}` : d.slice(0, 19));

function duration(from: string, to: string): string {
  const secs = Math.max(0, Math.round((new Date(to).getTime() - new Date(from).getTime()) / 1000));
  return secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m ${secs % 60}s`;
}

export function DeploymentSheet({ deploymentId, leaving, onClose }: { deploymentId: string; leaving?: boolean; onClose: () => void }) {
  const qc = useQueryClient();
  const [picked, setPicked] = useState<Tab | null>(null);
  const [initial, setInitial] = useState<Tab | null>(null);
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
  const isRepo = !!d?.commitSha;
  // The node writes a pre-deploy command's output to the build log too.
  const hasBuildLog = isRepo || !!d?.events.some((e) => e.message.startsWith("Running pre-deploy command"));
  // Settle the default once, so the tab doesn't jump when a build finishes.
  if (d && !initial) setInitial(d.status === "building" && d.commitSha ? "build" : "logs");
  const tab = picked ?? initial ?? "logs";

  return (
    <Sheet
      stacked
      leaving={leaving}
      onClose={onClose}
      header={
        d ? (
          <div className="flex flex-col gap-1.5">
            <div className="flex items-center gap-2.5">
              <Badge tone={deploymentBadge[d.status].tone}>{deploymentBadge[d.status].label}</Badge>
              <span className="truncate font-mono text-[13px]">{d.source || d.image}</span>
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
        onChange={setPicked}
        tabs={[
          { value: "details", label: "Details" },
          ...(hasBuildLog ? [{ value: "build" as const, label: isRepo ? "Build logs" : "Pre-deploy logs" }] : []),
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
                {d.commitSha ? (
                  <>
                    <dt className="text-muted-foreground">Commit</dt>
                    <dd className="flex min-w-0 items-center gap-1">
                      <span className="font-mono text-[13px]">{d.commitSha.slice(0, 7)}</span>
                      <CopyButton value={d.commitSha} label="Copy commit sha" />
                    </dd>
                    {d.commitMessage ? (
                      <>
                        <dt className="text-muted-foreground">Message</dt>
                        <dd className="line-clamp-3 whitespace-pre-line break-words">{d.commitMessage}</dd>
                      </>
                    ) : null}
                    {d.commitAuthor ? (
                      <>
                        <dt className="text-muted-foreground">Author</dt>
                        <dd className="truncate">{d.commitAuthor}</dd>
                      </>
                    ) : null}
                    <dt className="text-muted-foreground">Source</dt>
                    <dd className="truncate font-mono text-[13px]">{d.source}</dd>
                  </>
                ) : null}
                <dt className="text-muted-foreground">Image</dt>
                <dd className="truncate font-mono text-[13px]">{d.image}</dd>
                {d.imageDigest ? (
                  <>
                    <dt className="text-muted-foreground">Digest</dt>
                    <dd className="flex min-w-0 items-center gap-1">
                      <span className="truncate font-mono text-[13px]">{shortDigest(d.imageDigest)}</span>
                      <CopyButton value={d.imageDigest} label="Copy image digest" />
                    </dd>
                  </>
                ) : null}
                {d.buildStartedAt && d.buildFinishedAt ? (
                  <>
                    <dt className="text-muted-foreground">Build time</dt>
                    <dd>{duration(d.buildStartedAt, d.buildFinishedAt)}</dd>
                  </>
                ) : null}
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
                    <dd className="truncate font-mono text-[13px]" title={d.nodeId}>{d.nodeId}</dd>
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
        {hasBuildLog ? (
          <TabPanel value="build">
            <div className="flex h-full flex-col">
              <LogViewer url={api.deploymentLogsUrl(deploymentId, 500, "build")} downloadName={`deployment-${deploymentId}-build.log`} />
            </div>
          </TabPanel>
        ) : null}
        <TabPanel value="logs">
          <div className="flex h-full flex-col">
            <LogViewer url={api.deploymentLogsUrl(deploymentId)} downloadName={`deployment-${deploymentId}.log`} />
          </div>
        </TabPanel>
      </Tabs>
    </Sheet>
  );
}
