import * as Dialog from "@radix-ui/react-dialog";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/Button";
import { ArrowLeft, BrandDocker, BrandGithub, Bucket, ChevronRight, Search } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import { ServiceIcon } from "@/components/ui/ServiceIcon";
import { ApiError, api, type ProjectDetail, type Template } from "@/lib/api";
import { useTemplates } from "@/lib/useTemplates";
import { projectReturnPath } from "@/lib/github";
import { isDatabaseTemplate, suggestBucketName, templateStack, uniqueName } from "@/lib/templates";
import { useCloudflare } from "@/pages/settings/Integrations";
import { R2ChoiceFields, r2Choice, type R2Mode } from "./BucketFields";
import { BranchField, RepoField, useRepoSuggestions } from "./RepoFields";

/** `ghcr.io/acme/api:1.2` → `api`, made a valid service name. */
export function serviceNameFromImage(image: string): string {
  const last = image.split("@")[0]?.split("/").pop() ?? "";
  const name = (last.split(":")[0] ?? "")
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^[^a-z]+/, "")
    .replace(/-+$/, "")
    .slice(0, 63);
  return name || "service";
}

/** Accepts `owner/name`, `github.com/owner/name` or a full GitHub URL (`.git`, trailing paths
 *  and query strings tolerated) and returns `owner/name`, or null when it isn't one. */
export function parseRepo(input: string): string | null {
  const m = input
    .trim()
    .replace(/^(?:https?:\/\/)?(?:www\.)?github\.com\//i, "")
    .replace(/^git@github\.com:/i, "")
    .match(/^([A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?)\/([A-Za-z0-9._-]+?)(?:\.git)?(?:[/?#].*)?$/);
  return m ? `${m[1]}/${m[2]}` : null;
}

/** `acme/My_App` → `my-app`, made a valid service name. */
export const serviceNameFromRepo = (repo: string) => serviceNameFromImage(repo.split("/").pop() ?? "");

export type NewServiceStep = "root" | "image" | "repo" | "database" | "bucket";
type Step = NewServiceStep;

/** Command-style picker for adding a service (Docker image or GitHub repo). */
export function NewServiceDialog({
  open,
  onOpenChange,
  projectId,
  environmentId,
  onCreated,
  initialStep = "root",
  githubNotice = null,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  projectId: string;
  environmentId: string;
  onCreated: (serviceId: string) => void;
  /** Open straight at one source (the canvas menu), else the picker. */
  initialStep?: Step;
  /** How "Connect GitHub" went, shown on the GitHub step after coming back. */
  githubNotice?: { tone: "success" | "info"; text: string } | null;
}) {
  const qc = useQueryClient();
  const [step, setStep] = useState<Step>(initialStep);
  useEffect(() => {
    if (open) setStep(initialStep);
  }, [open, initialStep]);
  const [image, setImage] = useState("");
  const [name, setName] = useState("");
  const [port, setPort] = useState("8080");
  const [repoInput, setRepoInput] = useState("");
  const [branch, setBranch] = useState("main");
  const [rootDir, setRootDir] = useState("");
  const [notice, setNotice] = useState<string | null>(null);
  const [bucketKind, setBucketKind] = useState<"r2" | "self">("r2");
  const [r2Mode, setR2Mode] = useState<R2Mode>("new");
  // null: follow the service name.
  const [bucketName, setBucketName] = useState<string | null>(null);
  const [existingBucket, setExistingBucket] = useState("");

  const reset = () => {
    setStep("root");
    setImage("");
    setName("");
    setPort("8080");
    setRepoInput("");
    setBranch("main");
    setRootDir("");
    setNotice(null);
    setBucketKind("r2");
    setR2Mode("new");
    setBucketName(null);
    setExistingBucket("");
  };

  const repo = parseRepo(repoInput);
  const projectData = qc.getQueryData<ProjectDetail>(["project", projectId]);
  const workspaceId = projectData?.workspaceId;
  const envName = projectData?.environments.find((e) => e.id === environmentId)?.name;
  const takenNames = projectData?.services.map((s) => s.name) ?? [];
  const bucketService = name || uniqueName("bucket", takenNames);
  const bucketValue = bucketName ?? suggestBucketName(bucketService, envName);
  const cf = useCloudflare(workspaceId ?? "");
  const cfConnected = !!cf.data?.connected;
  const choice = r2Choice(r2Mode, bucketValue, existingBucket);
  const suggestions = useRepoSuggestions(
    open && step === "repo" ? workspaceId : undefined,
    repoInput,
    repo,
    branch,
    // "Connect GitHub" comes back here, with this dialog on the GitHub step.
    projectReturnPath(projectId, environmentId),
  );
  // The name picking a repository filled in; replaced on the next pick unless edited.
  const autoName = useRef("");

  const templates = useTemplates(open);

  const deployNew = async (svc: { instances: { id: string; environmentId: string }[] }) => {
    const instance = svc.instances.find((i) => i.environmentId === environmentId);
    // Deploy right away, like Railway; without a server the service just waits.
    if (instance) {
      try {
        await api.deploy(instance.id);
      } catch (e) {
        if (!(e instanceof ApiError && e.status === 409)) throw e;
        setNotice(e.message);
      }
    }
  };

  const createDatabase = useMutation({
    mutationFn: async (t: Template) => {
      const taken = qc.getQueryData<ProjectDetail>(["project", projectId])?.services.map((s) => s.name) ?? [];
      const svc = await api.createService(projectId, { name: uniqueName(t.defaultName, taken), template: t.id });
      await deployNew(svc);
      return svc;
    },
    onSuccess: async (svc) => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onCreated(svc.id);
      onOpenChange(false);
      reset();
    },
  });

  const createBucket = useMutation({
    mutationFn: async () => {
      if (bucketKind === "self") {
        const svc = await api.createService(projectId, { name: bucketService, template: "bucket" });
        await deployNew(svc);
        return svc;
      }
      if (!choice) throw new Error("Choose a bucket.");
      return api.createBucket(projectId, { name: bucketService, environmentId, ...choice });
    },
    onSuccess: async (svc) => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      void qc.invalidateQueries({ queryKey: ["cloudflare-r2-buckets"] });
      onCreated(svc.id);
      onOpenChange(false);
      reset();
    },
  });
  const dbTemplates = templates.data?.filter((t) => isDatabaseTemplate(t.id)) ?? [];

  const create = useMutation({
    mutationFn: async () => {
      const svc = await api.createService(
        projectId,
        step === "repo" && repo
          ? {
              name: name || serviceNameFromRepo(repo),
              config: {
                source: { type: "repo", repo, branch: branch.trim() || "main", rootDir: rootDir.trim().replace(/^\/+|\/+$/g, "") || undefined },
                port: Number(port) || 8080,
              },
            }
          : {
              name: name || serviceNameFromImage(image),
              config: { source: { type: "image", image: image.trim() }, port: Number(port) || 8080 },
            },
      );
      await deployNew(svc);
      return svc;
    },
    onSuccess: async (svc) => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onCreated(svc.id);
      onOpenChange(false);
      reset();
    },
  });

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) reset();
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50" />
        <Dialog.Content
          // Esc closes an open suggestion list first, not the dialog.
          onEscapeKeyDown={(e) => {
            if (document.activeElement?.getAttribute("aria-expanded") === "true") e.preventDefault();
          }}
          className="anim-modal fixed top-[16%] left-1/2 z-50 w-[480px] max-w-[calc(100vw-32px)] -translate-x-1/2 overflow-hidden rounded-2xl bg-popover text-popover-foreground shadow-modal outline-none">
          <Dialog.Title className="sr-only">Add a service</Dialog.Title>
          <Dialog.Description className="sr-only">Choose where the service comes from.</Dialog.Description>
          <div className="flex h-12 items-center gap-2 border-b border-border px-3 text-muted-foreground">
            {step === "root" ? (
              <Search className="size-4" />
            ) : (
              <Button variant="ghost" size="icon-sm" aria-label="Back" onClick={() => setStep("root")} className="-ml-1 text-muted-foreground">
                <ArrowLeft />
              </Button>
            )}
            <span className="text-sm text-foreground">
              {step === "root"
                ? "What would you like to add?"
                : step === "repo"
                  ? "GitHub repo"
                  : step === "database"
                    ? "Database"
                    : step === "bucket"
                      ? "Bucket"
                      : "Docker image"}
            </span>
          </div>

          {step === "root" || step === "database" ? (
            <div className="p-1.5">
              {step === "root" ? (
                <>
                  <PickerRow icon={<BrandGithub />} label="GitHub repo" onClick={() => setStep("repo")} />
                  <PickerRow icon={<BrandDocker />} label="Docker image" onClick={() => setStep("image")} />
                  <PickerRow icon={<Bucket />} label="Bucket" onClick={() => setStep("bucket")} />
                </>
              ) : null}
              {dbTemplates.length ? (
                <>
                  {step === "root" ? <div className="px-3 pt-3 pb-1 text-xs text-muted-foreground">Database</div> : null}
                  {createDatabase.error ? (
                    <div className="px-1.5 pb-1.5">
                      <Alert>{createDatabase.error.message}</Alert>
                    </div>
                  ) : null}
                  {notice ? (
                    <div className="px-1.5 pb-1.5">
                      <Alert tone="warn">{notice}</Alert>
                    </div>
                  ) : null}
                  {dbTemplates.map((t) => (
                    <PickerRow
                      key={t.id}
                      icon={<ServiceIcon stack={templateStack(t.id)} size="sm" />}
                      label={`Add ${t.name}`}
                      hint={createDatabase.isPending && createDatabase.variables?.id === t.id ? "Adding…" : undefined}
                      disabled={createDatabase.isPending}
                      onClick={() => createDatabase.mutate(t)}
                    />
                  ))}
                </>
              ) : null}
            </div>
          ) : step === "bucket" ? (
            <form
              className="flex flex-col gap-4 p-4"
              onSubmit={(e) => {
                e.preventDefault();
                createBucket.mutate();
              }}
            >
              {createBucket.error ? <Alert>{createBucket.error.message}</Alert> : null}
              {notice ? <Alert tone="warn">{notice}</Alert> : null}
              <Segmented
                label="Where"
                value={bucketKind}
                onChange={setBucketKind}
                options={[
                  { value: "r2", label: "Cloudflare R2" },
                  { value: "self", label: "Self-hosted" },
                ]}
              />
              {bucketKind === "self" ? (
                <p className="text-sm text-muted-foreground">
                  An S3-compatible server on your own servers, with the bucket on a volume. Other services reach it on the private network.
                </p>
              ) : cf.isPending && workspaceId ? null : !cfConnected ? (
                <Alert tone="info">
                  Connect Cloudflare in workspace Settings → Integrations to create R2 buckets. Each bucket gets its own credentials,
                  limited to that bucket.
                </Alert>
              ) : (
                <R2ChoiceFields
                  workspaceId={workspaceId}
                  mode={r2Mode}
                  onMode={setR2Mode}
                  bucket={bucketValue}
                  onBucket={setBucketName}
                  existing={existingBucket}
                  onExisting={setExistingBucket}
                />
              )}
              <Field
                label="Service name"
                hint={
                  <>
                    Others use <span className="font-mono">{`\${{${bucketService}.S3_BUCKET}}`}</span>, S3_ENDPOINT, S3_REGION,
                    AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY.
                  </>
                }
              >
                {(p) => <Input {...p} placeholder={bucketService} value={name} onChange={(e) => setName(e.target.value)} />}
              </Field>
              {bucketKind === "r2" && cfConnected ? (
                <p className="text-xs text-muted-foreground">
                  The bucket is for {envName ?? "this environment"}; other environments add theirs from the bucket&apos;s tab.
                </p>
              ) : null}
              <div className="flex justify-end gap-2">
                <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
                  Cancel
                </Button>
                <Button
                  type="submit"
                  variant="primary"
                  loading={createBucket.isPending}
                  disabled={bucketKind === "r2" && (!cfConnected || !choice)}
                >
                  Add bucket
                </Button>
              </div>
            </form>
          ) : step === "repo" ? (
            <form
              className="flex flex-col gap-4 p-4"
              onSubmit={(e) => {
                e.preventDefault();
                create.mutate();
              }}
            >
              {githubNotice ? <Alert tone={githubNotice.tone}>{githubNotice.text}</Alert> : null}
              {create.error ? <Alert>{create.error.message}</Alert> : null}
              {notice ? <Alert tone="warn">{notice}</Alert> : null}
              <RepoField
                s={suggestions}
                repoInput={repoInput}
                onRepoInput={setRepoInput}
                repo={repo}
                onPick={(r) => {
                  setRepoInput(r.fullName);
                  setBranch(r.defaultBranch);
                  if (!name || name === autoName.current) {
                    autoName.current = serviceNameFromRepo(r.fullName);
                    setName(autoName.current);
                  }
                }}
              />
              <div className="grid grid-cols-2 items-start gap-3">
                <BranchField s={suggestions} branch={branch} onBranch={setBranch} />
                <Field label="Root directory" hint="For monorepos. Optional.">
                  {(p) => <Input {...p} mono placeholder="/" value={rootDir} onChange={(e) => setRootDir(e.target.value)} />}
                </Field>
              </div>
              <div className="grid grid-cols-[1fr_120px] gap-3">
                <Field label="Service name">
                  {(p) => (
                    <Input {...p} placeholder={repo ? serviceNameFromRepo(repo) : "web"} value={name} onChange={(e) => setName(e.target.value)} />
                  )}
                </Field>
                <Field label="Port">
                  {(p) => (
                    <Input {...p} inputMode="numeric" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
                  )}
                </Field>
              </div>
              <div className="flex justify-end gap-2">
                <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
                  Cancel
                </Button>
                <Button type="submit" variant="primary" loading={create.isPending} disabled={!repo}>
                  Deploy
                </Button>
              </div>
            </form>
          ) : (
            <form
              className="flex flex-col gap-4 p-4"
              onSubmit={(e) => {
                e.preventDefault();
                create.mutate();
              }}
            >
              {create.error ? <Alert>{create.error.message}</Alert> : null}
              {notice ? <Alert tone="warn">{notice}</Alert> : null}
              <Field label="Image" hint="From Docker Hub, ghcr.io, quay.io or any registry, e.g. nginx:1.27">
                {(p) => (
                  <Input
                    {...p}
                    mono
                    autoFocus
                    required
                    placeholder="nginx:1.27"
                    value={image}
                    onChange={(e) => setImage(e.target.value)}
                  />
                )}
              </Field>
              <div className="grid grid-cols-[1fr_120px] gap-3">
                <Field label="Service name">
                  {(p) => (
                    <Input
                      {...p}
                      placeholder={image ? serviceNameFromImage(image) : "web"}
                      value={name}
                      onChange={(e) => setName(e.target.value)}
                    />
                  )}
                </Field>
                <Field label="Port">
                  {(p) => (
                    <Input {...p} inputMode="numeric" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
                  )}
                </Field>
              </div>
              <div className="flex justify-end gap-2">
                <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
                  Cancel
                </Button>
                <Button type="submit" variant="primary" loading={create.isPending} disabled={!image.trim()}>
                  Deploy
                </Button>
              </div>
            </form>
          )}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function PickerRow({
  icon,
  label,
  hint,
  disabled,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  hint?: string;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button
      disabled={!onClick || disabled}
      onClick={onClick}
      className="flex h-10 w-full cursor-pointer items-center gap-3 rounded-lg px-3 text-left text-sm hover:bg-accent disabled:cursor-default disabled:opacity-50 disabled:hover:bg-transparent [&>svg]:size-4 [&>svg]:text-muted-foreground"
    >
      {icon}
      <span className="flex-1">{label}</span>
      {hint ? <span className="text-xs text-muted-foreground">{hint}</span> : <ChevronRight />}
    </button>
  );
}
