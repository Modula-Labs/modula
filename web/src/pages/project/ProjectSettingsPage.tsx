import { Link } from "@tanstack/react-router";
import { Skeleton } from "@/components/ui/misc";
import { atLeast } from "@/lib/roles";
import { useProject } from "./ProjectContext";
import { DangerSection } from "./settings/DangerSection";
import { EnvironmentsSection } from "./settings/EnvironmentsSection";
import { GeneralSection } from "./settings/GeneralSection";
import { SETTINGS_SECTIONS, parseSection } from "./settings/sections";
import { SharedVariablesSection } from "./settings/SharedVariablesSection";
import { UsageSection } from "./settings/UsageSection";
import { WebhooksSection } from "./settings/WebhooksSection";
import { WorkspaceLinkSection } from "./settings/WorkspaceLinkSection";

/** Project Settings: section nav on the left, the section named in the URL on the right. */
export function ProjectSettingsPage({ projectId, section }: { projectId: string; section: string | undefined }) {
  const { project: p, env, role } = useProject();
  const active = parseSection(section);
  const canEdit = atLeast(role, "member");
  const canDeploy = atLeast(role, "deployer");
  const canAdmin = atLeast(role, "admin");

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto flex max-w-5xl flex-col gap-6 px-6 py-8 md:flex-row md:gap-10">
        <div className="md:w-48 md:shrink-0">
          <h1 className="text-xl font-semibold">Project Settings</h1>
          <nav aria-label="Settings sections" className="mt-4 md:sticky md:top-8">
            <ul className="flex gap-1 overflow-x-auto md:flex-col">
              {SETTINGS_SECTIONS.map((s) => (
                <li key={s.id}>
                  <Link
                    to="/projects/$projectId/settings/$section"
                    params={{ projectId, section: s.id }}
                    search={{ env: env?.id } as never}
                    aria-current={active === s.id ? "page" : undefined}
                    className={
                      active === s.id
                        ? "block rounded-md bg-accent px-2.5 py-1.5 text-sm font-medium whitespace-nowrap text-foreground"
                        : "block rounded-md px-2.5 py-1.5 text-sm whitespace-nowrap text-muted-foreground hover:bg-accent hover:text-foreground"
                    }
                  >
                    {s.label}
                  </Link>
                </li>
              ))}
            </ul>
          </nav>
        </div>
        <div className="min-w-0 flex-1">
          {!p ? (
            <Skeleton className="h-48" />
          ) : active === "general" ? (
            <GeneralSection key={p.id} project={p} canEdit={canEdit} />
          ) : active === "usage" ? (
            <UsageSection />
          ) : active === "environments" ? (
            <EnvironmentsSection project={p} current={env} canEdit={canEdit} canDelete={canAdmin} />
          ) : active === "shared-variables" ? (
            <SharedVariablesSection project={p} currentEnvId={env?.id} canEdit={canDeploy} />
          ) : active === "webhooks" ? (
            <WebhooksSection projectId={p.id} canEdit={canEdit} />
          ) : active === "members" ? (
            <WorkspaceLinkSection
              title="Members"
              description="Access is managed for the whole workspace, not per project. Everyone in the workspace sees its projects, according to their role."
              to="/settings"
              label="Open workspace settings"
            />
          ) : active === "tokens" ? (
            <WorkspaceLinkSection
              title="Tokens"
              description="API tokens belong to your account and can be limited to one workspace. Create and revoke them in your account security settings."
              to="/account/security"
              label="Open account security"
            />
          ) : active === "integrations" ? (
            <WorkspaceLinkSection
              title="Integrations"
              description="GitHub, Cloudflare, Hetzner and backup storage are connected once per workspace and used by all of its projects."
              to="/settings"
              label="Open workspace settings"
            />
          ) : (
            <DangerSection project={p} canEdit={canEdit} canDelete={canAdmin} />
          )}
        </div>
      </div>
    </div>
  );
}
