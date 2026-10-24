import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Card, CardContent, CardHeader } from "@/components/ui/Card";
import { CopyButton } from "@/components/ui/CopyButton";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { api, type Membership } from "@/lib/api";
import { useCurrentWorkspace, useMe } from "@/lib/workspace";
import { BackupStorage } from "./BackupStorage";
import { DomainSection } from "./settings/Domain";
import { IntegrationsSection } from "./settings/Integrations";
import { MembersSection } from "./settings/Members";
import { NotificationsSection } from "./settings/Notifications";
import { PrivateAccessSection } from "./settings/PrivateAccess";
import { UpdatesSection } from "./settings/Updates";

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
    <Card>
      <CardHeader
        title={<span className="font-bold">{title}</span>}
        description={description}
      />
      <CardContent className="flex min-w-0 flex-col gap-4">
        {children}
      </CardContent>
    </Card>
  );
}

/** Name, role and lifecycle of the current workspace. Only admins can change it. */
export function WorkspaceSettingsPage() {
  const { data: me } = useMe();
  const [workspace] = useCurrentWorkspace(me);

  return (
    <div>
      <h1 className="text-2xl leading-8 font-bold">Settings</h1>
      <p className="mt-1 text-sm text-muted-foreground">
        Manage this workspace.
      </p>
      <div className="mt-6">
        {workspace ? (
          <Settings workspace={workspace} />
        ) : (
          <Skeleton className="h-32" />
        )}
      </div>
    </div>
  );
}

function Settings({ workspace }: { workspace: Membership }) {
  const qc = useQueryClient();
  const { data: me } = useMe();
  const isAdmin = workspace.role === "admin";
  const [name, setName] = useState(workspace.name);
  const [deleting, setDeleting] = useState(false);
  useEffect(() => setName(workspace.name), [workspace.id, workspace.name]);

  const trimmed = name.trim();
  const dirty = trimmed !== workspace.name;
  const save = useMutation({
    mutationFn: () => api.updateWorkspace(workspace.id, trimmed),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["me"] }),
  });
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (dirty && trimmed) save.mutate();
  };

  return (
    <div className="flex flex-col gap-6">
      <Section
        title="General"
        description="How this workspace appears in the switcher."
      >
        <form onSubmit={submit} className="flex flex-col gap-4">
          {save.error ? <Alert>{save.error.message}</Alert> : null}
          <Field label="Name">
            {(p) => (
              <Input
                {...p}
                className="max-w-sm"
                disabled={!isAdmin}
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            )}
          </Field>
          {isAdmin ? (
            <div>
              <Button
                type="submit"
                variant="primary"
                loading={save.isPending}
                disabled={!dirty || !trimmed}
              >
                Save
              </Button>
            </div>
          ) : (
            <p className="text-[13px] text-muted-foreground">
              Only workspace admins can rename it.
            </p>
          )}
        </form>
      </Section>

      <Section
        title="Access"
        description="Your role here and the workspace's identifier."
      >
        <div className="flex items-center gap-3 text-sm">
          <span className="w-28 text-muted-foreground">Your role</span>
          <Badge>{workspace.role}</Badge>
        </div>
        <div className="flex items-center gap-3 text-sm">
          <span className="w-28 text-muted-foreground">Workspace ID</span>
          <code className="font-mono text-[13px]">{workspace.id}</code>
          <CopyButton value={workspace.id} label="Copy workspace ID" />
        </div>
      </Section>

      <div id="members" className="scroll-mt-4">
        <Section
          title="Members"
          description="Who can reach this workspace, and with which role. Invite people with a single-use link."
        >
          <MembersSection workspace={workspace} />
        </Section>
      </div>

      <Section
        title="Backup storage"
        description="S3-compatible buckets that database backups are written to."
      >
        <BackupStorage workspaceId={workspace.id} canEdit={isAdmin} />
      </Section>

      <Section
        title="Integrations"
        description="Optional providers this workspace can automate. Everything works without them."
      >
        <IntegrationsSection workspaceId={workspace.id} isAdmin={isAdmin} />
      </Section>

      <Section
        title="Notifications"
        description="Get told in Slack, Discord or a webhook when deployments fail or servers and services run hot."
      >
        <NotificationsSection workspaceId={workspace.id} />
      </Section>

      {me?.user.isInstanceAdmin && isAdmin ? (
        <Section
          title="Private dashboard"
          description="Cloudflare Tunnel + Access in front of the dashboard, using this workspace's Cloudflare connection. Instance admins only."
        >
          <PrivateAccessSection workspaceId={workspace.id} />
        </Section>
      ) : null}

      {me?.user.isInstanceAdmin ? (
        <div id="domain" className="scroll-mt-4">
          <Section
            title="Domain"
            description="Where the dashboard and your services are reachable. Instance admins only."
          >
            <DomainSection />
          </Section>
        </div>
      ) : null}

      {me?.user.isInstanceAdmin ? (
        <Section
          title="Updates"
          description="Modula Deploy versions for this instance. Instance admins only."
        >
          <UpdatesSection />
        </Section>
      ) : null}

      {isAdmin ? (
        <Section title="Danger zone">
          <div className="flex items-center justify-between gap-4 rounded-lg border border-destructive/40 px-3 py-2.5">
            <div className="text-sm">
              <div>Delete workspace</div>
              <div className="text-muted-foreground">
                Permanently removes its projects, services and members.
              </div>
            </div>
            <Button variant="danger" onClick={() => setDeleting(true)}>
              Delete workspace
            </Button>
          </div>
        </Section>
      ) : null}

      <DeleteDialog
        workspace={workspace}
        open={deleting}
        onOpenChange={setDeleting}
      />
    </div>
  );
}

function DeleteDialog({
  workspace,
  open,
  onOpenChange,
}: {
  workspace: Membership;
  open: boolean;
  onOpenChange: (o: boolean) => void;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [typed, setTyped] = useState("");
  const remove = useMutation({
    mutationFn: () => api.deleteWorkspace(workspace.id),
    onSuccess: async () => {
      onOpenChange(false);
      await qc.invalidateQueries({ queryKey: ["me"] });
      navigate({ to: "/projects" });
    },
  });
  return (
    <Modal
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) {
          setTyped("");
          remove.reset();
        }
      }}
      title="Delete workspace"
      description="This can't be undone. All projects, services and deployments in it are removed."
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (typed === workspace.name) remove.mutate();
        }}
      >
        {remove.error ? <Alert>{remove.error.message}</Alert> : null}
        <Field label={`Type ${workspace.name} to confirm`}>
          {(p) => (
            <Input
              {...p}
              autoFocus
              autoComplete="off"
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
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
            variant="danger"
            loading={remove.isPending}
            disabled={typed !== workspace.name}
          >
            Delete workspace
          </Button>
        </div>
      </form>
    </Modal>
  );
}
