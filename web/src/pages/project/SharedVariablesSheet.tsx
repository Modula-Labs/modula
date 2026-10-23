import { Sheet } from "@/components/ui/Sheet";
import { api } from "@/lib/api";
import { VariablesEditor, type VariablesScope } from "./VariablesTab";

/** The environment's shared variables: one set every service in it can read
 *  with `${{shared.NAME}}`. */
export function SharedVariablesSheet({
  environmentId,
  environmentName,
  canEdit,
  leaving,
  onClose,
}: {
  environmentId: string;
  environmentName: string;
  canEdit: boolean;
  leaving?: boolean;
  onClose: () => void;
}) {
  const scope: VariablesScope = {
    key: ["shared-variables", environmentId],
    environmentId,
    load: () => api.sharedVariables(environmentId),
    save: (b) => api.updateSharedVariables(environmentId, b),
  };
  return (
    <Sheet
      leaving={leaving}
      onClose={onClose}
      header={
        <div>
          <h2 className="truncate text-xl font-semibold">Shared variables</h2>
          <p className="mt-1 text-sm text-muted-foreground">Environment: {environmentName}</p>
        </div>
      }
    >
      <div className="min-h-0 flex-1 overflow-y-auto px-6 pb-6">
        <VariablesEditor scope={scope} canEdit={canEdit} noun="shared" />
      </div>
    </Sheet>
  );
}
