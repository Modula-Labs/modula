import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Plug, Rocket } from "@/components/ui/icons";
import { ServiceIcon } from "@/components/ui/ServiceIcon";
import { Sheet } from "@/components/ui/Sheet";
import { TabPanel, Tabs } from "@/components/ui/Tabs";
import { Alert, Skeleton } from "@/components/ui/misc";
import { api, type Role } from "@/lib/api";
import { useTemplates } from "@/lib/useTemplates";
import { templateStack } from "@/lib/templates";
import { atLeast } from "@/lib/roles";
import { BackupsTab } from "./BackupsTab";
import { ConnectDialog } from "./ConnectDialog";
import { DatabaseTab } from "./database/DatabaseTab";
import { DeploymentsTab } from "./DeploymentsTab";
import { MetricsTab } from "./MetricsTab";
import { SettingsTab } from "./SettingsTab";
import { VariablesTab } from "./VariablesTab";

export type ServiceTab = "deployments" | "metrics" | "database" | "variables" | "backups" | "settings";

export function ServiceSheet({
  instanceId,
  role,
  tab,
  deploymentId,
  onTab,
  onOpenDeployment,
  leaving,
  onClose,
}: {
  instanceId: string;
  role: Role | undefined;
  tab: ServiceTab;
  deploymentId?: string;
  onTab: (t: ServiceTab) => void;
  onOpenDeployment: (id: string) => void;
  leaving?: boolean;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const instance = useQuery({ queryKey: ["instance", instanceId], queryFn: () => api.serviceInstance(instanceId) });
  const deploy = useMutation({
    mutationFn: () => api.deploy(instanceId),
    onSuccess: (d) => {
      void qc.invalidateQueries({ queryKey: ["deployments", instanceId] });
      void qc.invalidateQueries({ queryKey: ["project"] });
      void qc.invalidateQueries({ queryKey: ["pending"] });
      onTab("deployments");
      onOpenDeployment(d.id);
    },
  });
  const i = instance.data;
  const [connecting, setConnecting] = useState(false);
  const templates = useTemplates(!!i?.template);
  const template = templates.data?.find((t) => t.id === i?.template);
  const canDeploy = atLeast(role, "deployer");

  return (
    <Sheet
      leaving={leaving}
      onClose={onClose}
      header={
        i ? (
          <div className="flex items-center gap-3">
            <ServiceIcon
              stack={templateStack(i.template) ?? i.stack}
              image={i.config.source.type === "image" ? i.config.source.image : null}
              repo={i.config.source.type === "repo" ? i.config.source.repo : null}
            />
            <h2 className="truncate text-xl leading-7 font-bold">{i.serviceName}</h2>
            <div className="ml-auto flex items-center gap-2 pr-2">
              {i.template ? (
                <Button variant="secondary" icon={<Plug />} onClick={() => setConnecting(true)}>
                  Connect
                </Button>
              ) : null}
              {canDeploy ? (
                <Button variant="primary" icon={<Rocket />} loading={deploy.isPending} onClick={() => deploy.mutate()}>
                  Deploy
                </Button>
              ) : null}
            </div>
          </div>
        ) : (
          <Skeleton className="h-8 w-56" />
        )
      }
    >
      {deploy.error ? (
        <div className="px-6 pb-3 md:px-10">
          <Alert>{deploy.error.message}</Alert>
        </div>
      ) : null}
      <Tabs
        value={tab}
        onChange={onTab}
        tabs={[
          { value: "deployments", label: "Deployments" },
          { value: "metrics", label: "Metrics" },
          ...(i?.template === "postgres" ? [{ value: "database" as const, label: "Database" }] : []),
          { value: "variables", label: "Variables" },
          ...(i?.template ? [{ value: "backups" as const, label: "Backups" }] : []),
          { value: "settings", label: "Settings" },
        ]}
      >
        <TabPanel value="deployments">
          <DeploymentsTab instanceId={instanceId} selected={deploymentId} canDeploy={canDeploy} onOpen={onOpenDeployment} />
        </TabPanel>
        <TabPanel value="metrics">
          <MetricsTab instanceId={instanceId} />
        </TabPanel>
        <TabPanel value="database">
          {i ? <DatabaseTab instance={i} canEdit={atLeast(role, "member")} onConnect={() => setConnecting(true)} /> : <Skeleton className="h-32" />}
        </TabPanel>
        <TabPanel value="variables">
          {i ? (
            <VariablesTab instanceId={instanceId} environmentId={i.environmentId} serviceId={i.serviceId} canEdit={canDeploy} />
          ) : (
            <Skeleton className="h-32" />
          )}
        </TabPanel>
        <TabPanel value="backups">
          {i ? <BackupsTab instance={i} canEdit={atLeast(role, "member")} /> : <Skeleton className="h-32" />}
        </TabPanel>
        <TabPanel value="settings">
          {i ? (
            <SettingsTab
              instance={i}
              canEdit={atLeast(role, "member")}
              canDelete={atLeast(role, "member")}
              onDeleted={onClose}
            />
          ) : (
            <Skeleton className="h-64" />
          )}
        </TabPanel>
      </Tabs>
      {i?.template ? (
        <ConnectDialog open={connecting} onOpenChange={setConnecting} instance={i} template={template} canEdit={atLeast(role, "member")} />
      ) : null}
    </Sheet>
  );
}
