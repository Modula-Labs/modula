import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { api, type Instance, type ProjectDetail, type ProjectVariableRef } from "@/lib/api";
import { serviceName } from "@/lib/templates";
import { ServiceNameField, serviceNameError } from "./ServiceNameField";
import { Section } from "./SettingsTab";

const where = (v: ProjectVariableRef) => `${v.environmentName} · ${v.serviceName ?? "shared"} · ${v.name}`;

/** The service's one name: shown everywhere, its private hostname and the
 *  prefix of references to its variables. Renaming rewrites those references
 *  in every environment (like Railway). */
export function ServiceNameSection({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const qc = useQueryClient();
  const saved = instance.serviceName;
  // Follows the saved name until edited (a rename elsewhere shows up).
  const [draft, setDraft] = useState<string | null>(null);
  const value = draft ?? saved;
  const [confirming, setConfirming] = useState(false);
  const next = serviceName(value);
  const taken =
    qc
      .getQueryData<ProjectDetail>(["project", instance.projectId])
      ?.services.filter((s) => s.id !== instance.serviceId)
      .map((s) => s.name) ?? [];
  const error = serviceNameError(next, taken);
  const dirty = next !== "" && next !== saved && !error;

  const preview = useQuery({
    queryKey: ["rename-preview", instance.serviceId],
    queryFn: () => api.renamePreview(instance.serviceId),
    enabled: confirming,
  });
  const rename = useMutation({
    mutationFn: () => api.renameService(instance.serviceId, next),
    onSuccess: async () => {
      setConfirming(false);
      setDraft(null);
      // Names show up in the canvas, references, suggestions and pending changes.
      await Promise.all(
        [["instance"], ["project"], ["pending"], ["variables"], ["shared-variables"], ["variable-names"], ["rename-preview"]].map(
          (queryKey) => qc.invalidateQueries({ queryKey }),
        ),
      );
    },
  });

  const refs = preview.data?.references ?? [];
  const similar = preview.data?.similarNames ?? [];
  return (
    <Section
      title="Service"
      description={
        <>
          Reached privately at <span className="font-mono">{saved}.modula.internal</span>; other services reference its
          variables as <span className="font-mono">{`\${{${saved}.NAME}}`}</span>.
        </>
      }
    >
      <div className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          <ServiceNameField
            label="Name"
            value={value}
            onChange={setDraft}
            taken={taken}
            disabled={!canEdit}
            hint="Lowercase letters, digits and -. Renaming rewrites references to it in every environment."
            onEnter={() => dirty && setConfirming(true)}
          />
        </div>
        {canEdit && dirty ? (
          <Button className="mt-6" variant="secondary" onClick={() => setConfirming(true)}>
            Rename
          </Button>
        ) : null}
      </div>
      <Modal
        open={confirming}
        onOpenChange={(o) => {
          setConfirming(o);
          if (!o) rename.reset();
        }}
        title={`Rename ${saved} to ${next}?`}
        description={
          <>
            Its private hostname becomes <span className="font-mono">{next}.modula.internal</span> on its next deploy;
            running services keep reaching the old name until they redeploy.
          </>
        }
        footer={
          <>
            <Button variant="ghost" onClick={() => setConfirming(false)}>
              Cancel
            </Button>
            <Button variant="primary" loading={rename.isPending} disabled={preview.isPending} onClick={() => rename.mutate()}>
              Rename
            </Button>
          </>
        }
      >
        {rename.error ? <Alert>{rename.error.message}</Alert> : null}
        {preview.error ? <Alert>{preview.error.message}</Alert> : null}
        {preview.isPending ? (
          <p className="text-sm text-muted-foreground">Looking for references…</p>
        ) : (
          <div className="flex flex-col gap-3 text-sm">
            <p>
              {refs.length === 0 ? (
                "No variables reference it."
              ) : (
                <>
                  {refs.length} {refs.length === 1 ? "variable references" : "variables reference"} it:{" "}
                  <span className="font-mono">{`\${{${saved}.…}}`}</span> becomes{" "}
                  <span className="font-mono">{`\${{${next}.…}}`}</span>, and the services using them get pending changes
                  to deploy.
                </>
              )}
            </p>
            {refs.length > 0 ? <NameList items={refs.map(where)} /> : null}
            {similar.length > 0 ? (
              <>
                <p className="text-muted-foreground">
                  These variable names contain the service&apos;s name; they stay as they are:
                </p>
                <NameList items={similar.map(where)} />
              </>
            ) : null}
          </div>
        )}
      </Modal>
    </Section>
  );
}

function NameList({ items }: { items: string[] }) {
  return (
    <ul className="max-h-40 overflow-y-auto rounded-lg border border-border px-3 py-2 font-mono text-xs">
      {items.map((s) => (
        <li key={s} className="truncate py-0.5">
          {s}
        </li>
      ))}
    </ul>
  );
}
