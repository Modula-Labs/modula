import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input, Select, Textarea } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import {
  api,
  type Builder,
  type Instance,
  type RestartPolicy,
  type ServiceConfig,
} from "@/lib/api";
import { DomainsSection } from "./DomainsSection";
import { TcpProxySection, VolumeSection } from "./StorageSections";

function Section({
  title,
  description,
  children,
}: {
  title: string;
  description?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="grid gap-4 @container border-b border-border py-6 first:pt-0 last:border-b-0">
      <div className="grid gap-4 @2xl:grid-cols-[200px_1fr] @2xl:gap-8">
        <div>
          <h3 className="text-base font-semibold">{title}</h3>
          {description ? (
            <p className="mt-1 text-sm text-muted-foreground">{description}</p>
          ) : null}
        </div>
        <div className="flex min-w-0 flex-col gap-4">{children}</div>
      </div>
    </section>
  );
}

/** Splits a start command into argv. Quotes group words; nothing is ever run
 *  through a shell. */
export function splitArgs(s: string): string[] {
  const out: string[] = [];
  const re = /"((?:\\.|[^"\\])*)"|'([^']*)'|(\S+)/g;
  for (const m of s.matchAll(re))
    out.push(
      m[1] !== undefined ? m[1].replace(/\\(.)/g, "$1") : (m[2] ?? m[3] ?? ""),
    );
  return out;
}

export function joinArgs(a: string[] | undefined): string {
  return (a ?? [])
    .map((x) => (/[\s"']/.test(x) || x === "" ? JSON.stringify(x) : x))
    .join(" ");
}

interface Form {
  kind: "image" | "repo" | "local";
  image: string;
  repo: string;
  branch: string;
  rootDir: string;
  builder: Builder;
  dockerfilePath: string;
  buildCommand: string;
  autoDeploy: boolean;
  watchPaths: string;
  startCommand: string;
  preDeployCommand: string;
  port: string;
  healthcheckPath: string;
  healthcheckTimeoutSecs: string;
  restartPolicy: RestartPolicy;
  maxRetries: string;
  drainingSeconds: string;
  memoryMb: string;
  cpuMillis: string;
  replicas: string;
}

export const toForm = (c: ServiceConfig): Form => ({
  kind: c.source.type,
  image: c.source.type === "image" ? c.source.image : "",
  repo: c.source.type === "repo" ? c.source.repo : "",
  branch: c.source.type === "repo" ? c.source.branch : "main",
  rootDir: c.source.type !== "image" ? (c.source.rootDir ?? "") : "",
  builder: c.builder ?? "railpack",
  dockerfilePath: c.dockerfilePath ?? "",
  buildCommand: c.buildCommand ?? "",
  autoDeploy: c.autoDeploy ?? true,
  watchPaths: (c.watchPaths ?? []).join("\n"),
  startCommand: joinArgs(c.startCommand),
  preDeployCommand: joinArgs(c.preDeployCommand),
  port: String(c.port),
  healthcheckPath: c.healthcheckPath ?? "",
  healthcheckTimeoutSecs: String(c.healthcheckTimeoutSecs),
  restartPolicy: c.restartPolicy,
  maxRetries: c.restartPolicyMaxRetries
    ? String(c.restartPolicyMaxRetries)
    : "",
  drainingSeconds: c.drainingSeconds ? String(c.drainingSeconds) : "",
  memoryMb: c.memoryMb ? String(c.memoryMb) : "",
  cpuMillis: c.cpuMillis ? String(c.cpuMillis) : "",
  replicas: String(c.replicas ?? 1),
});

const optNum = (s: string) => (s.trim() ? Number(s) : undefined);

/** The exact config shape the server accepts; build fields exist only for repo sources. */
export const toConfig = (f: Form): ServiceConfig => {
  const common = {
    startCommand: f.startCommand.trim() ? splitArgs(f.startCommand) : undefined,
    preDeployCommand: f.preDeployCommand.trim()
      ? splitArgs(f.preDeployCommand)
      : undefined,
    port: Number(f.port),
    healthcheckPath: f.healthcheckPath.trim() || undefined,
    healthcheckTimeoutSecs: Number(f.healthcheckTimeoutSecs) || 300,
    restartPolicy: f.restartPolicy,
    // A retry limit only exists for on-failure; 0 or empty means unlimited.
    restartPolicyMaxRetries:
      f.restartPolicy === "on_failure"
        ? optNum(f.maxRetries) || undefined
        : undefined,
    drainingSeconds: optNum(f.drainingSeconds) || undefined,
    memoryMb: optNum(f.memoryMb),
    cpuMillis: optNum(f.cpuMillis),
    // Multi-server replicas need private networking; stored but pinned to 1 for now.
    replicas: Math.max(1, Math.floor(Number(f.replicas)) || 1),
  };
  if (f.kind === "image")
    return { source: { type: "image", image: f.image.trim() }, ...common };
  const build = {
    builder: f.builder,
    dockerfilePath:
      f.builder === "dockerfile"
        ? f.dockerfilePath.trim() || undefined
        : undefined,
    buildCommand: f.buildCommand.trim() || undefined,
  };
  if (f.kind === "local")
    return {
      source: { type: "local", rootDir: f.rootDir.trim() || undefined },
      ...build,
      ...common,
    };
  const watchPaths = f.watchPaths
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  return {
    source: {
      type: "repo",
      repo: f.repo.trim(),
      branch: f.branch.trim() || "main",
      rootDir: f.rootDir.trim() || undefined,
    },
    ...build,
    autoDeploy: f.autoDeploy,
    watchPaths: watchPaths.length ? watchPaths : undefined,
    ...common,
  };
};

export function SettingsTab({
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
  const [form, setForm] = useState(() => toForm(instance.config));
  useEffect(() => setForm(toForm(instance.config)), [instance.config]);
  const dirty =
    JSON.stringify(toConfig(form)) !==
    JSON.stringify(toConfig(toForm(instance.config)));
  const set =
    <K extends keyof Form>(k: K) =>
    (e: { target: { value: string } }) =>
      setForm((f) => ({ ...f, [k]: e.target.value as Form[K] }));

  const save = useMutation({
    mutationFn: () => api.updateInstance(instance.id, toConfig(form)),
    onSuccess: (i) => {
      qc.setQueryData(["instance", instance.id], i);
      // The edit only takes effect on the next deploy.
      void qc.invalidateQueries({
        queryKey: ["pending", instance.environmentId],
      });
    },
  });
  const deploy = useMutation({
    mutationFn: () => api.deploy(instance.id),
    onSuccess: () => {
      void qc.invalidateQueries({
        queryKey: ["pending", instance.environmentId],
      });
      return qc.invalidateQueries({ queryKey: ["deployments", instance.id] });
    },
  });
  const remove = useMutation({
    mutationFn: () => api.deleteService(instance.serviceId),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["project", instance.projectId] });
      onDeleted();
    },
  });

  return (
    <div className="flex flex-col pb-20">
      {form.kind !== "image" ? (
        <Section
          title="Source"
          description={
            form.kind === "repo"
              ? "The repository every deploy builds from."
              : "Local files, uploaded with `modula up` from your machine. Deploying again reuses the latest upload."
          }
        >
          {form.kind === "repo" ? (
          <Field label="Repository">
            {(p) => (
              <Input
                {...p}
                mono
                disabled={!canEdit}
                value={form.repo}
                onChange={set("repo")}
              />
            )}
          </Field>
          ) : null}
          <div className="grid grid-cols-2 gap-3">
            {form.kind === "repo" ? (
            <Field label="Branch">
              {(p) => (
                <Input
                  {...p}
                  mono
                  disabled={!canEdit}
                  value={form.branch}
                  onChange={set("branch")}
                />
              )}
            </Field>
            ) : null}
            <Field label="Root directory" hint="For monorepos.">
              {(p) => (
                <Input
                  {...p}
                  mono
                  disabled={!canEdit}
                  placeholder="/"
                  value={form.rootDir}
                  onChange={set("rootDir")}
                />
              )}
            </Field>
          </div>
          <div className="flex flex-col gap-1.5">
            <div className="text-sm font-medium">Builder</div>
            <div
              className={canEdit ? undefined : "pointer-events-none opacity-50"}
            >
              <Segmented
                label="Builder"
                value={form.builder}
                onChange={(builder) => setForm((f) => ({ ...f, builder }))}
                options={[
                  { value: "railpack", label: "Railpack" },
                  { value: "dockerfile", label: "Dockerfile" },
                ]}
              />
            </div>
            <p className="text-xs text-muted-foreground">
              {form.builder === "railpack"
                ? "Detects your language and builds without a Dockerfile."
                : form.kind === "repo"
                  ? "Builds with a Dockerfile from the repository."
                  : "Builds with the Dockerfile in the uploaded files."}
            </p>
          </div>
          {form.builder === "dockerfile" ? (
            <Field
              label="Dockerfile path"
              hint="Relative to the root directory. Defaults to Dockerfile."
            >
              {(p) => (
                <Input
                  {...p}
                  mono
                  disabled={!canEdit}
                  placeholder="Dockerfile"
                  value={form.dockerfilePath}
                  onChange={set("dockerfilePath")}
                />
              )}
            </Field>
          ) : null}
          <Field
            label="Build command"
            hint="Leave empty to use the detected default."
          >
            {(p) => (
              <Input
                {...p}
                mono
                disabled={!canEdit}
                placeholder="npm run build"
                value={form.buildCommand}
                onChange={set("buildCommand")}
              />
            )}
          </Field>
          {form.kind === "repo" ? (
          <>
          <label className="flex items-start gap-2.5 text-sm">
            <input
              type="checkbox"
              disabled={!canEdit}
              checked={form.autoDeploy}
              onChange={(e) =>
                setForm((f) => ({ ...f, autoDeploy: e.target.checked }))
              }
              className="mt-0.5 size-4 accent-foreground"
            />
            <span>
              <span className="font-medium">Auto-deploy</span>
              <span className="block text-xs text-muted-foreground">
                Deploy automatically when the branch gets new commits.
              </span>
            </span>
          </label>
          <Field
            label="Watch paths"
            hint="One glob per line. Only matching changes trigger a deploy. Empty means any change."
          >
            {(p) => (
              <Textarea
                {...p}
                disabled={!canEdit}
                placeholder={"src/**\n!docs/**"}
                value={form.watchPaths}
                onChange={set("watchPaths")}
              />
            )}
          </Field>
          </>
          ) : null}
        </Section>
      ) : (
        <Section title="Source" description="The image every deploy pulls.">
          <Field label="Image">
            {(p) => (
              <Input
                {...p}
                mono
                disabled={!canEdit}
                value={form.image}
                onChange={set("image")}
              />
            )}
          </Field>
        </Section>
      )}

      <Section
        title="Networking"
        description="Generated and custom domains. HTTPS is set up automatically."
      >
        <DomainsSection instanceId={instance.id} canEdit={canEdit} />
        <TcpProxySection instance={instance} canEdit={canEdit} />
        <Field
          label="Port"
          hint="The port your app listens on. It's also set as PORT."
        >
          {(p) => (
            <Input
              {...p}
              className="w-32"
              inputMode="numeric"
              disabled={!canEdit}
              value={form.port}
              onChange={set("port")}
            />
          )}
        </Field>
      </Section>

      <Section title="Deploy">
        <Field
          label="Pre-deploy command"
          hint="Runs once in the new image before it starts, e.g. migrations. If it fails, the current deployment keeps running."
        >
          {(p) => (
            <Input
              {...p}
              mono
              disabled={!canEdit}
              placeholder="npx prisma migrate deploy"
              value={form.preDeployCommand}
              onChange={set("preDeployCommand")}
            />
          )}
        </Field>
        <Field
          label="Start command"
          hint="Leave empty to use the default. Never run through a shell."
        >
          {(p) => (
            <Input
              {...p}
              mono
              disabled={!canEdit}
              placeholder="node server.js"
              value={form.startCommand}
              onChange={set("startCommand")}
            />
          )}
        </Field>
        <div className="grid grid-cols-[1fr_160px] gap-3">
          <Field
            label="Health check path"
            hint="A deploy goes live once this returns 2xx."
          >
            {(p) => (
              <Input
                {...p}
                mono
                disabled={!canEdit}
                placeholder="/health"
                value={form.healthcheckPath}
                onChange={set("healthcheckPath")}
              />
            )}
          </Field>
          <Field label="Timeout (seconds)">
            {(p) => (
              <Input
                {...p}
                inputMode="numeric"
                disabled={!canEdit}
                value={form.healthcheckTimeoutSecs}
                onChange={set("healthcheckTimeoutSecs")}
              />
            )}
          </Field>
        </div>
        <div className="flex flex-wrap items-start gap-3">
          <Field label="Restart policy">
            {(p) => (
              <Select
                {...p}
                className="w-56"
                disabled={!canEdit}
                value={form.restartPolicy}
                onChange={set("restartPolicy")}
              >
                <option value="on_failure">On failure</option>
                <option value="always">Always</option>
                <option value="never">Never</option>
              </Select>
            )}
          </Field>
          {form.restartPolicy === "on_failure" ? (
            <Field label="Max retries" hint="Empty means no limit.">
              {(p) => (
                <Input
                  {...p}
                  className="w-32"
                  inputMode="numeric"
                  disabled={!canEdit}
                  placeholder="No limit"
                  value={form.maxRetries}
                  onChange={set("maxRetries")}
                />
              )}
            </Field>
          ) : null}
        </div>
      </Section>

      <Section
        title="Teardown"
        description="How a replaced deployment is stopped."
      >
        <Field
          label="Draining seconds"
          hint="Time between SIGTERM and SIGKILL, up to 300. Empty uses 10 seconds."
        >
          {(p) => (
            <Input
              {...p}
              className="w-32"
              inputMode="numeric"
              disabled={!canEdit}
              placeholder="10"
              value={form.drainingSeconds}
              onChange={set("drainingSeconds")}
            />
          )}
        </Field>
      </Section>

      <Section
        title="Resources"
        description="Hard limits for the container. Empty means no limit."
      >
        <div className="grid grid-cols-2 gap-3">
          <Field label="Memory (MB)">
            {(p) => (
              <Input
                {...p}
                inputMode="numeric"
                disabled={!canEdit}
                placeholder="No limit"
                value={form.memoryMb}
                onChange={set("memoryMb")}
              />
            )}
          </Field>
          <Field label="CPU (millicores)" hint="1000 = one vCPU.">
            {(p) => (
              <Input
                {...p}
                inputMode="numeric"
                disabled={!canEdit}
                placeholder="No limit"
                value={form.cpuMillis}
                onChange={set("cpuMillis")}
              />
            )}
          </Field>
        </div>
        <Field
          label="Replicas"
          hint="Each replica runs on a different server. On one server, raise CPU and memory instead."
          error={
            Number(form.replicas) > 1
              ? "Multiple replicas need private networking between servers (coming soon)"
              : null
          }
        >
          {(p) => (
            <Input
              {...p}
              type="number"
              min={1}
              max={1}
              inputMode="numeric"
              disabled={!canEdit}
              value={form.replicas}
              onChange={set("replicas")}
            />
          )}
        </Field>
      </Section>

      <Section
        title="Volume"
        description="Persistent storage that survives redeploys."
      >
        <VolumeSection instance={instance} canEdit={canEdit} />
      </Section>

      {canDelete ? (
        <Section title="Danger zone">
          {remove.error ? <Alert>{remove.error.message}</Alert> : null}
          <div className="flex items-center justify-between gap-4 rounded-lg border border-destructive/40 p-4">
            <div className="text-sm">
              <div className="font-medium">Delete service</div>
              <div className="text-muted-foreground">
                Removes it from every environment and stops its containers.
              </div>
            </div>
            <Button
              variant="danger"
              loading={remove.isPending}
              onClick={() => remove.mutate()}
            >
              Delete service
            </Button>
          </div>
        </Section>
      ) : null}

      {canEdit && (dirty || save.isSuccess || save.error || deploy.error) ? (
        <div className="sticky bottom-0 mt-4 flex items-center justify-between gap-3 rounded-lg border border-border bg-popover px-4 py-3 shadow-menu">
          <div className="min-w-0 text-sm">
            {save.error ? (
              <span className="text-error">{save.error.message}</span>
            ) : deploy.error ? (
              <span className="text-error">{deploy.error.message}</span>
            ) : dirty ? (
              "Unsaved changes"
            ) : (
              "Saved. Deploy to apply the new settings."
            )}
          </div>
          <div className="flex shrink-0 gap-2">
            {dirty ? (
              <>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => setForm(toForm(instance.config))}
                >
                  Discard
                </Button>
                <Button
                  size="sm"
                  variant="secondary"
                  loading={save.isPending}
                  disabled={Number(form.replicas) > 1}
                  onClick={() => save.mutate()}
                >
                  Save
                </Button>
              </>
            ) : (
              <Button
                size="sm"
                variant="secondary"
                loading={deploy.isPending}
                onClick={() =>
                  deploy.mutate(undefined, { onSuccess: () => save.reset() })
                }
              >
                Deploy
              </Button>
            )}
          </div>
        </div>
      ) : null}
    </div>
  );
}
