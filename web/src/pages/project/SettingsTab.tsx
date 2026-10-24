import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Checkbox } from "@/components/ui/Controls";
import { Field, Input, Select, Textarea } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import {
  api,
  type Builder,
  type ProjectDetail,
  type Instance,
  type RestartPolicy,
  type ServiceConfig,
} from "@/lib/api";
import { projectReturnPath } from "@/lib/github";
import { hostingError } from "@/lib/hosting";
import { CronFields } from "./CronFields";
import { DomainsSection } from "./DomainsSection";
import { HostingFields } from "./HostingSection";
import { parseRepo } from "./NewServiceDialog";
import { BranchField, RepoField, useRepoSuggestions } from "./RepoFields";
import { ResourceSliders, SectionRail, ServerPicker, useNodes } from "./SettingsParts";
import { TcpProxySection, VolumeSection } from "./StorageSections";

const sectionId = (title: string) => `settings-${title.toLowerCase().replace(/[^a-z0-9]+/g, "-")}`;

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
    <section id={sectionId(title)} className="grid scroll-mt-2 gap-4 @container border-b border-border py-8 first:pt-0 last:border-b-0">
      <div className="grid gap-4 @2xl:grid-cols-[200px_1fr] @2xl:gap-8">
        <div>
          <h3 className="text-base leading-6 font-bold">{title}</h3>
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

export interface Form {
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
  waitForCi: boolean;
  startCommand: string;
  preDeployCommand: string;
  port: string;
  healthcheckPath: string;
  healthcheckTimeoutSecs: string;
  restartPolicy: RestartPolicy;
  maxRetries: string;
  drainingSeconds: string;
  cronSchedule: string;
  cronTimeout: string;
  memoryMb: string;
  cpuMillis: string;
  replicas: string;
  cloudflare: boolean;
  outputDir: string;
  spa: boolean;
  scriptName: string;
}

// R2 buckets (source `r2`) have no settings form: the sheet never shows it.
export const toForm = (c: ServiceConfig): Form => ({
  kind: c.source.type === "r2" ? "image" : c.source.type,
  image: c.source.type === "image" ? c.source.image : "",
  repo: c.source.type === "repo" ? c.source.repo : "",
  branch: c.source.type === "repo" ? c.source.branch : "main",
  rootDir: c.source.type === "repo" || c.source.type === "local" ? (c.source.rootDir ?? "") : "",
  builder: c.builder ?? "railpack",
  dockerfilePath: c.dockerfilePath ?? "",
  buildCommand: c.buildCommand ?? "",
  autoDeploy: c.autoDeploy ?? true,
  watchPaths: (c.watchPaths ?? []).join("\n"),
  waitForCi: c.waitForCi ?? false,
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
  cronSchedule: c.cronSchedule ?? "",
  cronTimeout: c.cronTimeoutSecs ? String(c.cronTimeoutSecs) : "",
  memoryMb: c.memoryMb ? String(c.memoryMb) : "",
  cpuMillis: c.cpuMillis ? String(c.cpuMillis) : "",
  replicas: String(c.replicas ?? 1),
  cloudflare: c.hosting?.type === "cloudflare",
  outputDir: c.hosting?.outputDir ?? "",
  spa: c.hosting?.spa ?? false,
  scriptName: c.hosting?.scriptName ?? "",
});

/** Switches the service's source kind, keeping every other field. Fields that
 *  belong to the kind being left stay in the form, so switching back restores them. */
export const switchSource = (f: Form, kind: Form["kind"]): Form => ({
  ...f,
  kind,
});

const optNum = (s: string) => (s.trim() ? Number(s) : undefined);

/** The exact config shape the server accepts; build fields exist only for repo sources. */
/** Each replica on its own server (the server enforces the same). */
export const MAX_REPLICAS = 16;

/** Why `replicas` can't be saved, or null. A volume's data is on one server. */
export function replicasError(
  replicas: string,
  hasVolume: boolean,
): string | null {
  const n = Number(replicas);
  if (!Number.isInteger(n) || n < 1 || n > MAX_REPLICAS)
    return `Between 1 and ${MAX_REPLICAS}`;
  if (n > 1 && hasVolume)
    return "A service with a volume runs one replica: raise CPU and memory instead";
  return null;
}

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
    cronSchedule: f.cronSchedule.trim() || undefined,
    // The timeout only exists with a schedule; empty means the server default.
    cronTimeoutSecs: f.cronSchedule.trim() ? optNum(f.cronTimeout) || undefined : undefined,
    memoryMb: optNum(f.memoryMb),
    cpuMillis: optNum(f.cpuMillis),
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
    hosting: f.cloudflare
      ? {
          type: "cloudflare" as const,
          outputDir: f.outputDir.trim(),
          spa: f.spa,
          scriptName: f.scriptName.trim() || undefined,
        }
      : undefined,
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
      repo: parseRepo(f.repo) ?? f.repo.trim(),
      branch: f.branch.trim() || "main",
      rootDir: f.rootDir.trim() || undefined,
    },
    ...build,
    autoDeploy: f.autoDeploy,
    watchPaths: watchPaths.length ? watchPaths : undefined,
    waitForCi: f.waitForCi || undefined,
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
  useEffect(() => {
    setForm(toForm(instance.config));
    setConfirmDisconnect(false);
  }, [instance.config]);
  const dirty =
    JSON.stringify(toConfig(form)) !==
    JSON.stringify(toConfig(toForm(instance.config)));
  const origKind = toForm(instance.config).kind;
  const [confirmDisconnect, setConfirmDisconnect] = useState(false);
  const workspaceId = qc.getQueryData<ProjectDetail>([
    "project",
    instance.projectId,
  ])?.workspaceId;
  const suggestions = useRepoSuggestions(
    form.kind === "repo" && canEdit ? workspaceId : undefined,
    form.repo,
    parseRepo(form.repo),
    form.branch,
    projectReturnPath(instance.projectId, instance.environmentId),
  );
  const nodes = useNodes();
  const node = nodes.data?.find((n) => n.id === instance.nodeId);
  const railItems = [
    "Source",
    ...(form.kind !== "image" ? ["Hosting"] : []),
    "Networking",
    "Deploy",
    "Cron schedule",
    "Teardown",
    "Resources",
    "Regions & Replicas",
    "Volume",
    ...(canDelete ? ["Danger zone"] : []),
  ].map((label) => ({ id: sectionId(label), label }));
  const switchKind = (kind: Form["kind"]) => setForm((f) => switchSource(f, kind));
  const sourceDescription =
    form.kind === "repo"
      ? "The repository every deploy builds from."
      : form.kind === "image"
        ? "The image every deploy pulls."
        : (
          <>
            Deployed from local files with <code className="font-mono text-foreground">modula up</code>. Deploying again
            reuses the latest upload.
          </>
        );
  const invalidSource =
    form.kind === "repo" && !parseRepo(form.repo)
      ? "Pick a repository (owner/name) first."
      : form.kind !== "image"
        ? hostingError(form)
        : null;
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
    <div className="@container flex items-start gap-8">
    <div className="flex min-w-0 flex-1 flex-col pb-20">
      <Section title="Source" description={sourceDescription}>
        {form.kind === "image" ? (
          <>
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
            {canEdit ? (
              <div>
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => switchKind("repo")}
                >
                  Use a GitHub repository instead
                </Button>
              </div>
            ) : null}
          </>
        ) : null}
        {form.kind === "local" && canEdit ? (
          <div>
            <Button
              size="sm"
              variant="secondary"
              onClick={() => switchKind("repo")}
            >
              Connect a GitHub repository
            </Button>
          </div>
        ) : null}
        {form.kind === "repo" ? (
          <>
            <RepoField
              s={suggestions}
              repoInput={form.repo}
              onRepoInput={(repo) => setForm((f) => ({ ...f, repo }))}
              repo={parseRepo(form.repo)}
              autoFocus={origKind !== "repo"}
              disabled={!canEdit}
              onPick={(r) =>
                setForm((f) => ({
                  ...f,
                  repo: r.fullName,
                  branch: r.defaultBranch,
                }))
              }
            />
          </>
        ) : null}
        {form.kind !== "image" ? (
          <>
            <div className="grid grid-cols-2 items-start gap-3">
              {form.kind === "repo" ? (
                <BranchField
                  s={suggestions}
                  branch={form.branch}
                  disabled={!canEdit}
                  onBranch={(branch) => setForm((f) => ({ ...f, branch }))}
                />
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
            <div className="flex flex-col gap-2">
              <div className="text-sm leading-none font-normal">Builder</div>
              <div
                className={
                  canEdit ? undefined : "pointer-events-none opacity-50"
                }
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
              <p className="text-sm text-muted-foreground">
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
          </>
        ) : null}
        {form.kind === "repo" ? (
          <>
            <label className="flex items-start gap-2 text-sm">
              <Checkbox
                disabled={!canEdit}
                checked={form.autoDeploy}
                onCheckedChange={(autoDeploy) =>
                  setForm((f) => ({ ...f, autoDeploy }))
                }
                className="mt-0.5"
              />
              <span className="flex flex-col gap-1">
                <span className="leading-none font-normal">Auto-deploy</span>
                <span className="text-sm text-muted-foreground">
                  Deploy automatically when the branch gets new commits.
                </span>
              </span>
            </label>
            <label className="flex items-start gap-2 text-sm">
              <Checkbox
                disabled={!canEdit}
                checked={form.waitForCi}
                onCheckedChange={(waitForCi) =>
                  setForm((f) => ({ ...f, waitForCi }))
                }
                className="mt-0.5"
              />
              <span className="flex flex-col gap-1">
                <span className="leading-none font-normal">Wait for CI</span>
                <span className="text-sm text-muted-foreground">
                  Build a pushed commit only once its other GitHub checks pass.
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
        {form.kind === "repo" && canEdit ? (
          confirmDisconnect ? (
            <div className="flex flex-col gap-3 rounded-xl border border-border p-4 text-sm">
              <p className="text-muted-foreground">
                Future deploys will come from local files with{" "}
                <code className="font-mono text-foreground">modula up</code>,
                and push-to-deploy stops.
              </p>
              <div className="flex gap-2">
                <Button
                  size="sm"
                  variant="secondary"
                  onClick={() => {
                    switchKind("local");
                    setConfirmDisconnect(false);
                  }}
                >
                  Switch to local files
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => setConfirmDisconnect(false)}
                >
                  Cancel
                </Button>
              </div>
            </div>
          ) : (
            <div>
              <Button
                size="sm"
                variant="ghost"
                onClick={() =>
                  origKind === "repo"
                    ? setConfirmDisconnect(true)
                    : switchKind(origKind)
                }
              >
                {origKind === "repo"
                  ? "Disconnect repository"
                  : "Cancel connecting a repository"}
              </Button>
            </div>
          )
        ) : null}
      </Section>

      {form.kind !== "image" ? (
        <Section title="Hosting" description="Where this site is served from.">
          <HostingFields
            form={form}
            setForm={setForm}
            canEdit={canEdit}
            workspaceId={workspaceId}
            serviceName={instance.serviceName}
            environmentName={instance.environmentName}
          />
        </Section>
      ) : null}

      <Section
        title="Networking"
        description="Generated and custom domains. HTTPS is set up automatically."
      >
        <DomainsSection
          instanceId={instance.id}
          canEdit={canEdit}
          cloudflare={instance.config.hosting?.type === "cloudflare"}
        />
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
        title="Cron schedule"
        description="Run the service on a schedule instead of continuously."
      >
        <CronFields
          schedule={form.cronSchedule}
          timeout={form.cronTimeout}
          canEdit={canEdit}
          onSchedule={(cronSchedule) => setForm((f) => ({ ...f, cronSchedule }))}
          onTimeout={(cronTimeout) => setForm((f) => ({ ...f, cronTimeout }))}
        />
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
        description="Hard limits for the container. Unset, a service gets 1 vCPU and 512 MB."
      >
        <ResourceSliders
          node={node}
          canEdit={canEdit}
          cpuMillis={form.cpuMillis}
          memoryMb={form.memoryMb}
          onCpu={(cpuMillis) => setForm((f) => ({ ...f, cpuMillis }))}
          onMemory={(memoryMb) => setForm((f) => ({ ...f, memoryMb }))}
        />
      </Section>

      <Section
        title="Regions & Replicas"
        description="The server this service runs on, and how many copies of it."
      >
        <ServerPicker
          instance={instance}
          nodes={nodes.data}
          workspaceId={workspaceId}
          canEdit={canEdit}
        />
        <Field
          label="Replicas"
          hint="Each replica runs on a different server, and every one of them serves the service's domains. On one server, raise CPU and memory instead."
          error={replicasError(form.replicas, !!instance.volume)}
        >
          {(p) => (
            <Input
              {...p}
              className="w-32"
              type="number"
              min={1}
              max={MAX_REPLICAS}
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
          <div className="flex items-center justify-between gap-4 rounded-xl border border-destructive/40 p-4">
            <div className="text-sm">
              <div className="font-bold">Delete service</div>
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
        <div className="sticky bottom-0 mt-4 flex items-center justify-between gap-3 rounded-xl border border-border bg-popover px-4 py-3 shadow-menu">
          <div className="min-w-0 text-sm">
            {save.error ? (
              <span className="text-error">{save.error.message}</span>
            ) : deploy.error ? (
              <span className="text-error">{deploy.error.message}</span>
            ) : dirty ? (
              (invalidSource ?? "Unsaved changes")
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
                  disabled={
                    !!replicasError(form.replicas, !!instance.volume) ||
                    !!invalidSource
                  }
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
    <SectionRail items={railItems} />
    </div>
  );
}
