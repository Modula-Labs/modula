import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { LayoutGrid, Plus, Search, X } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Alert, EmptyState, Skeleton } from "@/components/ui/misc";
import { Modal } from "@/components/ui/Modal";
import { ServiceIcon } from "@/components/ui/ServiceIcon";
import { api, type ProjectCard } from "@/lib/api";
import { dismissDomainBanner, domainBannerDismissed } from "@/lib/domain";
import { relativeTime } from "@/lib/format";
import { useCurrentWorkspace, useMe } from "@/lib/workspace";
import { useInstanceDomain } from "./settings/Domain";

/** At most this many service icons; the rest are a "+N". */
const MAX_ICONS = 6;

/** The services as icon tiles on the dot grid, and how many are online. */
function CardPreview({ project: p }: { project: ProjectCard }) {
  const online = p.services.filter((s) => s.online).length;
  const total = p.services.length;
  const dot =
    total === 0 || online === 0
      ? "bg-muted-foreground"
      : online === total
        ? "bg-success"
        : "bg-warning";
  return (
    <div className="dotgrid mx-4 mb-4 mt-3 flex flex-1 flex-col rounded-lg border border-border">
      <div className="flex flex-1 flex-wrap items-center justify-center gap-2 px-4">
        {p.services.slice(0, MAX_ICONS).map((s) => (
          <span key={s.id} title={s.name} className="rounded-lg shadow-sm">
            <ServiceIcon
              stack={s.stack}
              image={s.image}
              repo={s.repo}
              size="lg"
            />
          </span>
        ))}
        {total > MAX_ICONS ? (
          <span className="flex size-10 items-center justify-center rounded-lg border border-border bg-card text-xs text-muted-foreground">
            +{total - MAX_ICONS}
          </span>
        ) : null}
      </div>
      <div className="flex items-center gap-2 px-3 pb-2.5 text-xs text-muted-foreground">
        {total === 0 ? (
          <span>No services yet · created {relativeTime(p.createdAt)}</span>
        ) : (
          <>
            <span className={`size-1.5 rounded-full ${dot}`} />
            {p.environment ? <span>{p.environment}</span> : null}
            <span aria-hidden>·</span>
            <span>
              {online}/{total} services online
            </span>
          </>
        )}
      </div>
    </div>
  );
}

function DomainBanner() {
  const [hidden, setHidden] = useState(domainBannerDismissed);
  const q = useInstanceDomain();
  if (hidden || !q.data || q.data.dashboard) return null;
  return (
    <div className="mb-6 flex items-center gap-3 rounded-xl border border-border bg-card px-4 py-3 text-sm">
      <span className="min-w-0 flex-1">
        The dashboard is served at the server's IP address. Add a domain for a
        proper address and generated service URLs.{" "}
        <Link to="/settings" hash="domain" className="font-normal underline underline-offset-4">
          Settings → Domain
        </Link>
      </span>
      <Button
        variant="ghost"
        size="icon-sm"
        aria-label="Dismiss"
        className="text-muted-foreground"
        onClick={() => {
          dismissDomainBanner();
          setHidden(true);
        }}
      >
        <X />
      </Button>
    </div>
  );
}

export function ProjectsPage() {
  const { data: me } = useMe();
  const [workspace] = useCurrentWorkspace(me);
  const [query, setQuery] = useState("");
  const [creating, setCreating] = useState(false);
  const projects = useQuery({
    queryKey: ["projects", workspace?.id],
    queryFn: () => api.projects(workspace!.id),
    enabled: !!workspace,
  });
  const canCreate =
    workspace && workspace.role !== "viewer" && workspace.role !== "deployer";
  const shown = (projects.data ?? []).filter((p) =>
    p.name.toLowerCase().includes(query.trim().toLowerCase()),
  );

  return (
    <div>
      {me?.user.isInstanceAdmin ? <DomainBanner /> : null}
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-2xl leading-8 font-bold">
          Projects
        </h1>
        <div className="flex items-center gap-2">
          <label className="flex h-8 w-60 items-center gap-2 rounded-lg border border-input px-2.5 text-muted-foreground focus-within:border-ring dark:bg-input/30">
            <Search className="size-4" />
            <input
              className="min-w-0 flex-1 bg-transparent text-sm text-foreground outline-none placeholder:text-muted-foreground"
              placeholder="Search projects…"
              aria-label="Search projects"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </label>
          {canCreate ? (
            <Button
              variant="primary"
              icon={<Plus />}
              onClick={() => setCreating(true)}
            >
              New
            </Button>
          ) : null}
        </div>
      </div>
      <div className="mt-6 flex items-center border-b border-border pb-3 text-[13px] text-muted-foreground">
        <span className="flex items-center gap-1.5">
          <LayoutGrid className="size-3.5" /> {projects.data?.length ?? 0}{" "}
          projects
        </span>
      </div>

      <div className="mt-6">
        {projects.isPending ? (
          <div className="grid grid-cols-2 gap-4">
            <Skeleton className="h-48" />
            <Skeleton className="h-48" />
          </div>
        ) : projects.data && projects.data.length === 0 ? (
          <EmptyState
            icon={<LayoutGrid />}
            title="Create your first project"
            body="Projects group the services, databases and environments of one app."
            action={
              canCreate ? (
                <Button
                  variant="outline"
                  icon={<Plus />}
                  onClick={() => setCreating(true)}
                >
                  New project
                </Button>
              ) : undefined
            }
          />
        ) : (
          <div className="grid grid-cols-2 gap-4">
            {shown.map((p) => (
              <Link
                key={p.id}
                to="/projects/$projectId"
                params={{ projectId: p.id }}
                className="group flex h-64 flex-col overflow-hidden rounded-xl border border-border bg-card transition-colors hover:border-foreground/25"
              >
                <div className="px-4 pt-4">
                  <div className="text-base font-bold">{p.name}</div>
                  {p.description ? (
                    <div className="mt-0.5 truncate text-sm text-muted-foreground">
                      {p.description}
                    </div>
                  ) : null}
                </div>
                <CardPreview project={p} />
              </Link>
            ))}
          </div>
        )}
      </div>
      {workspace ? (
        <NewProjectDialog
          open={creating}
          onOpenChange={setCreating}
          workspace={workspace.id}
        />
      ) : null}
    </div>
  );
}

function NewProjectDialog({
  open,
  onOpenChange,
  workspace,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  workspace: string;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const create = useMutation({
    mutationFn: () => api.createProject(workspace, { name }),
    onSuccess: async (p) => {
      await qc.invalidateQueries({ queryKey: ["projects", workspace] });
      onOpenChange(false);
      setName("");
      await navigate({
        to: "/projects/$projectId",
        params: { projectId: p.id },
      });
    },
  });
  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      title="New project"
      description="It starts with a production environment."
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          create.mutate();
        }}
      >
        {create.error ? <Alert>{create.error.message}</Alert> : null}
        <Field label="Name">
          {(p) => (
            <Input
              {...p}
              autoFocus
              required
              maxLength={64}
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          )}
        </Field>
        <div className="flex justify-end gap-2">
          <Button
            type="button"
            variant="ghost"
            onClick={() => onOpenChange(false)}
          >
            Cancel
          </Button>
          <Button
            type="submit"
            variant="primary"
            loading={create.isPending}
            disabled={!name.trim()}
          >
            Create project
          </Button>
        </div>
      </form>
    </Modal>
  );
}
