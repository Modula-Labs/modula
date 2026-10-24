import { api } from "@/lib/api";
import { VariablesEditor, type VariablesScope } from "./VariablesTab";

/** An environment's shared variables: one set every service in it can read
 *  with `${{shared.NAME}}`. */
export function SharedVariables({ environmentId, canEdit }: { environmentId: string; canEdit: boolean }) {
  const scope: VariablesScope = {
    key: ["shared-variables", environmentId],
    environmentId,
    load: () => api.sharedVariables(environmentId),
    save: (b) => api.updateSharedVariables(environmentId, b),
  };
  return <VariablesEditor scope={scope} canEdit={canEdit} noun="shared" />;
}
