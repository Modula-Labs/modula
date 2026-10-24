import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Alert, Badge } from "@/components/ui/misc";
import type { EdgeRule } from "@/lib/api";
import { describeAction, unsupportedMessage } from "@/lib/edge";
import { EdgeRulesDialog } from "./EdgeRulesDialog";

/** The Edge settings section: an empty state or a compact list of the saved rules, and the dialog that edits them. */
export function EdgeSection({
  rules,
  unsupported,
  canEdit,
  onSave,
}: {
  rules: EdgeRule[];
  /** Action types a server running this service can't apply. */
  unsupported: string[];
  canEdit: boolean;
  onSave: (rules: EdgeRule[]) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  return (
    <>
      {unsupported.length ? <Alert tone="warn">{unsupportedMessage(unsupported)}</Alert> : null}
      {rules.length === 0 ? (
        <div className="flex flex-col items-start gap-3 rounded-xl border border-dashed border-border p-4">
          <div>
            <div className="text-sm font-bold">No edge rules</div>
            <p className="text-sm text-muted-foreground">Requests go straight to your service.</p>
          </div>
          {canEdit ? (
            <Button variant="outline" onClick={() => setOpen(true)}>
              Add rule
            </Button>
          ) : null}
        </div>
      ) : (
        <div className="flex flex-col gap-3">
          <ol className="flex flex-col divide-y divide-border rounded-xl border border-border">
            {rules.map((r, i) => (
              <li key={i} className="flex items-center gap-3 px-3 py-2 text-sm">
                <span className="w-4 shrink-0 text-muted-foreground tabular-nums">{i + 1}</span>
                <span className={r.name ? "min-w-0 flex-1 truncate" : "min-w-0 flex-1 truncate text-muted-foreground"}>{r.name || "Unnamed rule"}</span>
                <Badge>{describeAction(r.then)}</Badge>
                {r.enabled ? null : <Badge>Disabled</Badge>}
              </li>
            ))}
          </ol>
          {canEdit ? (
            <div>
              <Button variant="outline" onClick={() => setOpen(true)}>
                Edit rules
              </Button>
            </div>
          ) : null}
        </div>
      )}
      <EdgeRulesDialog open={open} onOpenChange={setOpen} rules={rules} onSave={onSave} />
    </>
  );
}
