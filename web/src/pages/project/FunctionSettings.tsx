import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { api, type Instance, type ProjectDetail, type ServiceConfig } from "@/lib/api";
import {
  codeError,
  compatibilityDateError,
  DEFAULT_ENTRY,
  entryError,
  functionTarget,
  hostingFor,
  type FunctionSource,
  type FunctionTarget,
} from "@/lib/functions";
import { defaultScriptName, validateScriptName, workersDevHost } from "@/lib/hosting";
import { cloudflare } from "@/lib/integrations";
import { DomainsSection } from "./DomainsSection";
import { CodeEditor, CodeSize, TargetField } from "./FunctionFields";
import { parseRepo } from "./NewServiceDialog";
import { BranchField, RepoField, useRepoSuggestions } from "./RepoFields";
import { projectReturnPath } from "@/lib/github";
import { SectionRail } from "./SettingsParts";
import { Section, sectionId } from "./SettingsTab";

export interface FunctionForm {
  /** Bundled from a repository rather than written here. */
  fromRepo: boolean;
  code: string;
  repo: string;
  branch: string;
  rootDir: string;
  entry: string;
  compatibilityDate: string;
  target: FunctionTarget;
  scriptName: string;
  healthcheckPath: string;
  replicas: string;
}

export const toFunctionForm = (c: ServiceConfig): FunctionForm => {
  const s = c.source as FunctionSource;
  return {
    fromRepo: !!s.repo,
    code: s.code ?? "",
    repo: s.repo ?? "",
    branch: s.branch ?? "main",
    rootDir: s.rootDir ?? "",
    entry: s.entry ?? "",
    compatibilityDate: s.compatibilityDate,
    target: functionTarget(c),
    scriptName: c.hosting?.scriptName ?? "",
    healthcheckPath: c.healthcheckPath ?? "",
    replicas: String(c.replicas ?? 1),
  };
};

/** The instance's config with the form's settings; everything else stays as it is. */
export const fromFunctionForm = (base: ServiceConfig, f: FunctionForm): ServiceConfig => {
  const source: FunctionSource = f.fromRepo
    ? {
        type: "function",
        repo: parseRepo(f.repo) ?? f.repo.trim(),
        branch: f.branch.trim() || "main",
        rootDir: f.rootDir.trim().replace(/^\/+|\/+$/g, "") || undefined,
        entry: f.entry.trim() || undefined,
        compatibilityDate: f.compatibilityDate.trim(),
      }
    : { type: "function", code: f.code, compatibilityDate: f.compatibilityDate.trim() };
  const cloud = f.target === "cloudflare";
  return {
    ...base,
    source,
    hosting: hostingFor(f.target, f.scriptName),
    healthcheckPath: !cloud ? f.healthcheckPath.trim() || undefined : undefined,
    replicas: cloud ? 1 : Math.max(1, Math.floor(Number(f.replicas)) || 1),
  };
};

/** The first reason the form can't be saved, or null. */
export function functionFormError(f: FunctionForm): string | null {
  if (f.fromRepo) {
    if (!parseRepo(f.repo)) return "Pick a repository (owner/name) first.";
    const e = entryError(f.entry);
    if (e) return `Entry file: ${e}`;
  } else {
    const e = codeError(f.code);
    if (e) return e;
  }
  const d = compatibilityDateError(f.compatibilityDate.trim(), f.target);
  if (d) return `Compatibility date: ${d}`;
  if (f.target === "cloudflare") {
    const n = validateScriptName(f.scriptName);
    if (n) return `Worker name: ${n}`;
  }
  return null;
}

/** Settings of a function service (`source.type === "function"`). */
export function FunctionSettings({
  instance,
  canEdit,
  canDelete,
  onDeleted,
}: {
  instance: Instance;
  canEdit: boolean;
  canDelete: boolean;
  onDeleted: () => void;
}) {
  const qc = useQueryClient();
  const [form, setForm] = useState(() => toFunctionForm(instance.config));
  useEffect(() => setForm(toFunctionForm(instance.config)), [instance.config]);
  const patch = (p: Partial<FunctionForm>) => setForm((f) => ({ ...f, ...p }));
  const next = fromFunctionForm(instance.config, form);
  const dirty = JSON.stringify(next) !== JSON.stringify(fromFunctionForm(instance.config, toFunctionForm(instance.config)));
  const invalid = functionFormError(form);
  const workspaceId = qc.getQueryData<ProjectDetail>(["project", instance.projectId])?.workspaceId;
  const status = useQuery({
    queryKey: ["cloudflare", workspaceId],
    queryFn: () => cloudflare.status(workspaceId!),
    enabled: !!workspaceId,
  });
  const suggestions = useRepoSuggestions(
    form.fromRepo && canEdit ? workspaceId : undefined,
    form.repo,
    parseRepo(form.repo),
    form.branch,
    projectReturnPath(instance.projectId, instance.environmentId),
  );
  const cloud = form.target === "cloudflare";
  const defaultName = defaultScriptName(instance.serviceName, instance.environmentName);

  const save = useMutation({
    mutationFn: () => api.updateInstance(instance.id, next),
    onSuccess: (i) => {
      qc.setQueryData(["instance", instance.id], i);
      void qc.invalidateQueries({ queryKey: ["pending", instance.environmentId] });
    },
  });
  const remove = useMutation({
    mutationFn: () => api.deleteService(instance.serviceId),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["project", instance.projectId] });
      onDeleted();
    },
  });
  const rail = ["Code", "Runtime", "Networking", ...(cloud ? [] : ["Deploy"]), ...(canDelete ? ["Danger zone"] : [])].map(
    (label) => ({ id: sectionId(label), label }),
  );

  return (
    <div className="@container flex items-start gap-8">
      <div className="flex min-w-0 flex-1 flex-col pb-20">
        <Section
          title="Code"
          description={
            form.fromRepo
              ? "Every deploy installs the repository's dependencies and bundles the entry file with esbuild."
              : "A module worker in JavaScript. Variables are on env; console output is the deployment's logs."
          }
        >
          {form.fromRepo ? (
            <>
              <RepoField
                s={suggestions}
                repoInput={form.repo}
                onRepoInput={(repo) => patch({ repo })}
                repo={parseRepo(form.repo)}
                disabled={!canEdit}
                onPick={(r) => patch({ repo: r.fullName, branch: r.defaultBranch })}
              />
              <div className="grid grid-cols-2 items-start gap-3">
                <BranchField s={suggestions} branch={form.branch} disabled={!canEdit} onBranch={(branch) => patch({ branch })} />
                <Field label="Root directory" hint="For monorepos.">
                  {(p) => (
                    <Input {...p} mono disabled={!canEdit} placeholder="/" value={form.rootDir} onChange={(e) => patch({ rootDir: e.target.value })} />
                  )}
                </Field>
              </div>
              <Field label="Entry file" hint="Relative to the root directory. TypeScript is fine." error={entryError(form.entry)}>
                {(p) => (
                  <Input {...p} mono disabled={!canEdit} placeholder={DEFAULT_ENTRY} value={form.entry} onChange={(e) => patch({ entry: e.target.value })} />
                )}
              </Field>
            </>
          ) : (
            <>
              <Field label="Code" error={form.code ? codeError(form.code) : null}>
                {(p) => <CodeEditor {...p} disabled={!canEdit} value={form.code} onChange={(code) => patch({ code })} rows={22} />}
              </Field>
              <div className="-mt-2 flex justify-end">
                <CodeSize code={form.code} />
              </div>
            </>
          )}
          {canEdit ? (
            <div>
              <Button size="sm" variant="ghost" onClick={() => patch({ fromRepo: !form.fromRepo })}>
                {form.fromRepo ? "Write the code here instead" : "Bundle from a GitHub repository instead"}
              </Button>
            </div>
          ) : null}
        </Section>

        <Section title="Runtime" description="Where the function runs, and the Workers runtime version it is written for.">
          <TargetField
            target={form.target}
            onTarget={(target) => patch({ target })}
            connected={status.isPending && workspaceId ? undefined : status.data?.connected === true}
            canEdit={canEdit}
          />
          {cloud ? (
            <Field
              label="Worker name"
              hint={
                <>
                  Served at <span className="font-mono">{workersDevHost(form.scriptName.trim() || defaultName)}</span> and on your custom domains.
                </>
              }
              error={validateScriptName(form.scriptName)}
            >
              {(p) => (
                <Input {...p} mono disabled={!canEdit} placeholder={defaultName} value={form.scriptName} onChange={(e) => patch({ scriptName: e.target.value })} />
              )}
            </Field>
          ) : null}
          <Field
            label="Compatibility date"
            hint="Runtime behaviour as of this date (Cloudflare's compatibility dates)."
            error={compatibilityDateError(form.compatibilityDate.trim(), form.target)}
          >
            {(p) => (
              <Input
                {...p}
                mono
                className="w-40"
                disabled={!canEdit}
                placeholder="2026-10-01"
                value={form.compatibilityDate}
                onChange={(e) => patch({ compatibilityDate: e.target.value })}
              />
            )}
          </Field>
        </Section>

        <Section title="Networking" description="Generated and custom domains. HTTPS is set up automatically.">
          <DomainsSection instanceId={instance.id} canEdit={canEdit} cloudflare={instance.config.hosting?.type === "cloudflare"} />
        </Section>

        {!cloud ? (
          <Section title="Deploy" description="A new version serves once workerd has loaded it.">
            <Field label="Health check path" hint="Optional. A deploy goes live once this returns 2xx.">
              {(p) => (
                <Input
                  {...p}
                  mono
                  disabled={!canEdit}
                  placeholder="/health"
                  value={form.healthcheckPath}
                  onChange={(e) => patch({ healthcheckPath: e.target.value })}
                />
              )}
            </Field>
            <Field label="Replicas" hint="Each replica runs on a different server, in its workerd.">
              {(p) => (
                <Input
                  {...p}
                  className="w-32"
                  type="number"
                  min={1}
                  max={16}
                  inputMode="numeric"
                  disabled={!canEdit}
                  value={form.replicas}
                  onChange={(e) => patch({ replicas: e.target.value })}
                />
              )}
            </Field>
          </Section>
        ) : null}

        {canDelete ? (
          <Section title="Danger zone">
            {remove.error ? <Alert>{remove.error.message}</Alert> : null}
            <div className="flex items-center justify-between gap-4 rounded-xl border border-destructive/40 p-4">
              <div className="text-sm">
                <div className="font-bold">Delete service</div>
                <div className="text-muted-foreground">Removes it from every environment and stops it everywhere it runs.</div>
              </div>
              <Button variant="danger" loading={remove.isPending} onClick={() => remove.mutate()}>
                Delete service
              </Button>
            </div>
          </Section>
        ) : null}

        {canEdit && (dirty || save.isSuccess || save.error) ? (
          <div className="sticky bottom-0 mt-4 flex items-center justify-between gap-3 rounded-xl border border-border bg-popover px-4 py-3 shadow-menu">
            <div className="min-w-0 text-sm">
              {save.error ? (
                <span className="text-error">{save.error.message}</span>
              ) : dirty ? (
                (invalid ?? "Unsaved changes")
              ) : (
                "Saved. Deploy the pending changes at the top to apply it."
              )}
            </div>
            <div className="flex shrink-0 gap-2">
              {dirty ? (
                <>
                  <Button size="sm" variant="ghost" onClick={() => setForm(toFunctionForm(instance.config))}>
                    Discard
                  </Button>
                  <Button size="sm" variant="secondary" loading={save.isPending} disabled={!!invalid} onClick={() => save.mutate()}>
                    Save
                  </Button>
                </>
              ) : (
                <Button size="sm" variant="ghost" onClick={() => save.reset()}>
                  Dismiss
                </Button>
              )}
            </div>
          </div>
        ) : null}
      </div>
      <SectionRail items={rail} />
    </div>
  );
}
