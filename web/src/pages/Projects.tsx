import { LayoutGrid, List, Plus, Search } from "lucide-react";
import { Button } from "@/components/ui/Button";
import { EmptyState, Kbd } from "@/components/ui/misc";

export function ProjectsPage() {
  return (
    <div>
      <div className="flex items-center justify-between gap-4">
        <h1 className="text-[30px] leading-9 font-normal tracking-[-0.01em]">Projects</h1>
        <div className="flex items-center gap-2">
          <label className="flex h-8 w-60 items-center gap-2 rounded-sm border border-border-strong bg-surface px-2.5 text-fg-subtle focus-within:border-accent">
            <Search className="size-4" />
            <input className="min-w-0 flex-1 bg-transparent text-sm text-fg outline-none placeholder:text-fg-subtle" placeholder="Search projects…" aria-label="Search projects" disabled />
            <Kbd>⌘</Kbd><Kbd>K</Kbd>
          </label>
          <Button variant="primary" icon={<Plus className="size-4" />} disabled title="Project creation arrives with the canvas">
            New
          </Button>
        </div>
      </div>
      <div className="mt-6 flex items-center justify-between border-b border-border pb-3 text-xs text-fg-muted">
        <span className="flex items-center gap-1.5"><LayoutGrid className="size-3.5" /> 0 projects</span>
        <span className="flex items-center gap-1 rounded-sm bg-surface-3 p-0.5" role="group" aria-label="Layout">
          <span className="rounded-xs border border-border bg-surface p-1"><LayoutGrid className="size-3.5" /></span>
          <span className="p-1"><List className="size-3.5" /></span>
        </span>
      </div>
      <div className="mt-6">
        <EmptyState
          icon={<LayoutGrid />}
          title="Create your first project"
          body="Projects group the services, databases and environments of one app. Deploying from a Docker image or GitHub arrives in the next milestones."
        />
      </div>
    </div>
  );
}
