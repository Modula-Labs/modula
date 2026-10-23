import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { LayoutGrid, Plus, Search } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Alert, EmptyState, Skeleton } from "@/components/ui/misc";
import { Modal } from "@/components/ui/Modal";
import { api } from "@/lib/api";
import { relativeTime } from "@/lib/format";
import { useCurrentWorkspace, useMe } from "@/lib/workspace";

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
  const canCreate = workspace && workspace.role !== "viewer" && workspace.role !== "deployer";
  const shown = (projects.data ?? []).filter((p) => p.name.toLowerCase().includes(query.trim().toLowerCase()));

  return (
    <div>
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-[30px] leading-9 font-normal tracking-[-0.01em]">Projects</h1>
        <div className="flex items-center gap-2">
          <label className="flex h-9 w-60 items-center gap-2 rounded-md border border-input px-2.5 text-muted-foreground focus-within:border-ring">
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
            <Button variant="primary" icon={<Plus />} onClick={() => setCreating(true)}>
              New
            </Button>
          ) : null}
        </div>
      </div>
      <div className="mt-6 flex items-center border-b border-border pb-3 text-xs text-muted-foreground">
        <span className="flex items-center gap-1.5">
          <LayoutGrid className="size-3.5" /> {projects.data?.length ?? 0} projects
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
                <Button variant="outline" icon={<Plus />} onClick={() => setCreating(true)}>
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
                className="group flex h-48 flex-col overflow-hidden rounded-xl border border-border bg-card hover:border-input"
              >
                <div className="px-5 pt-4">
                  <div className="text-sm font-semibold">{p.name}</div>
                  {p.description ? (
                    <div className="mt-0.5 truncate text-sm text-muted-foreground">{p.description}</div>
                  ) : null}
                </div>
                <div className="dotgrid mx-5 my-3 flex-1 rounded-lg" />
                <div className="px-5 pb-4 text-xs text-muted-foreground">Created {relativeTime(p.createdAt)}</div>
              </Link>
            ))}
          </div>
        )}
      </div>
      {workspace ? <NewProjectDialog open={creating} onOpenChange={setCreating} workspace={workspace.id} /> : null}
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
      await navigate({ to: "/projects/$projectId", params: { projectId: p.id } });
    },
  });
  return (
    <Modal open={open} onOpenChange={onOpenChange} title="New project" description="It starts with a production environment.">
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          create.mutate();
        }}
      >
        {create.error ? <Alert>{create.error.message}</Alert> : null}
        <Field label="Name">
          {(p) => <Input {...p} autoFocus required maxLength={64} value={name} onChange={(e) => setName(e.target.value)} />}
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending} disabled={!name.trim()}>
            Create project
          </Button>
        </div>
      </form>
    </Modal>
  );
}
