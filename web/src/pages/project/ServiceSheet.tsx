import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/Button";
import { Rocket } from "@/components/ui/icons";
import { ServiceIcon } from "@/components/ui/ServiceIcon";
import { Sheet } from "@/components/ui/Sheet";
import { TabPanel, Tabs } from "@/components/ui/Tabs";
import { Alert, Skeleton } from "@/components/ui/misc";
import { api, type Role } from "@/lib/api";
import { atLeast } from "@/lib/roles";
import { DeploymentsTab } from "./DeploymentsTab";
import { SettingsTab } from "./SettingsTab";
import { VariablesTab } from "./VariablesTab";

export type ServiceTab = "deployments" | "variables" | "settings";

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
  const canDeploy = atLeast(role, "deployer");

  return (
    <Sheet
      leaving={leaving}
      onClose={onClose}
      header={
        i ? (
          <div className="flex items-center gap-3">
            <ServiceIcon
              stack={i.stack}
              image={i.config.source.type === "image" ? i.config.source.image : null}
              repo={i.config.source.type === "repo" ? i.config.source.repo : null}
            />
            <h2 className="truncate text-xl font-semibold">{i.serviceName}</h2>
            <div className="ml-auto pr-2">
              {canDeploy ? (
                <Button size="sm" variant="secondary" icon={<Rocket />} loading={deploy.isPending} onClick={() => deploy.mutate()}>
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
          { value: "variables", label: "Variables" },
          { value: "settings", label: "Settings" },
        ]}
      >
        <TabPanel value="deployments">
          <DeploymentsTab instanceId={instanceId} selected={deploymentId} canDeploy={canDeploy} onOpen={onOpenDeployment} />
        </TabPanel>
        <TabPanel value="variables">
          {i ? (
            <VariablesTab instanceId={instanceId} environmentId={i.environmentId} serviceId={i.serviceId} canEdit={canDeploy} />
          ) : (
            <Skeleton className="h-32" />
          )}
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
    </Sheet>
  );
}
