import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Field, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/Table";
import { Plus } from "@/components/ui/icons";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type Member, type Membership, type Role } from "@/lib/api";
import { relativeTime } from "@/lib/format";
import { ROLES, ROLE_HINTS, expiresIn, isLastAdmin, roleLabel } from "@/lib/members";
import { atLeast } from "@/lib/roles";
import { useMe } from "@/lib/workspace";

const EXPIRY_CHOICES = [1, 3, 7, 14, 30];

const message = (e: unknown, fallback: string) => (e instanceof ApiError ? e.message : fallback);

/** Workspace members and invite links. Admins change roles, remove members and invite. */
export function MembersSection({ workspace }: { workspace: Membership }) {
  const qc = useQueryClient();
  const { data: me } = useMe();
  const isAdmin = atLeast(workspace.role, "admin");
  const members = useQuery({ queryKey: ["members", workspace.id], queryFn: () => api.members(workspace.id) });
  const [removing, setRemoving] = useState<Member | null>(null);
  const [leaving, setLeaving] = useState(false);
  const [inviting, setInviting] = useState(false);

  const updateRole = useMutation({
    mutationFn: ({ id, role }: { id: string; role: Role }) => api.updateMember(id, role),
    onSettled: () => qc.invalidateQueries({ queryKey: ["members", workspace.id] }),
  });

  const list = members.data ?? [];
  const self = list.find((m) => m.userId === me?.user.id);

  return (
    <div className="flex flex-col gap-4">
      {updateRole.error ? <Alert>{message(updateRole.error, "Could not change the role.")}</Alert> : null}
      {members.isLoading ? (
        <Skeleton className="h-24" />
      ) : members.error ? (
        <Alert>{message(members.error, "Could not load members.")}</Alert>
      ) : (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Role</TableHead>
              <TableHead>Joined</TableHead>
              <TableHead>2FA</TableHead>
              <TableHead className="w-24" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {list.map((m) => {
              const isSelf = m.userId === me?.user.id;
              const last = isLastAdmin(list, m);
              return (
                <TableRow key={m.id}>
                  <TableCell>
                    <div className="flex flex-col">
                      <span>
                        {m.name}
                        {isSelf ? <span className="text-muted-foreground"> (you)</span> : null}
                      </span>
                      <span className="text-[13px] text-muted-foreground">{m.email}</span>
                    </div>
                  </TableCell>
                  <TableCell>
                    {isAdmin ? (
                      <Select
                        aria-label={`Role of ${m.name}`}
                        className="w-32"
                        value={m.role}
                        disabled={last || (updateRole.isPending && updateRole.variables?.id === m.id)}
                        title={last ? "A workspace needs at least one admin" : undefined}
                        onChange={(e) => updateRole.mutate({ id: m.id, role: e.target.value as Role })}
                      >
                        {ROLES.map((r) => (
                          <option key={r} value={r}>
                            {roleLabel(r)}
                          </option>
                        ))}
                      </Select>
                    ) : (
                      <Badge>{roleLabel(m.role)}</Badge>
                    )}
                  </TableCell>
                  <TableCell className="text-muted-foreground">{relativeTime(m.joinedAt)}</TableCell>
                  <TableCell>{m.twoFactor ? <Badge tone="success">On</Badge> : <Badge>Off</Badge>}</TableCell>
                  <TableCell className="text-right">
                    {isSelf ? (
                      <Button size="sm" variant="ghost" disabled={last} title={last ? "A workspace needs at least one admin" : undefined} onClick={() => setLeaving(true)}>
                        Leave
                      </Button>
                    ) : isAdmin ? (
                      <Button size="sm" variant="ghost" disabled={last} onClick={() => setRemoving(m)}>
                        Remove
                      </Button>
                    ) : null}
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      )}

      {isAdmin ? (
        <>
          <PendingInvites workspaceId={workspace.id} />
          <div>
            <Button variant="outline" icon={<Plus />} onClick={() => setInviting(true)}>
              Invite
            </Button>
          </div>
          <InviteDialog workspaceId={workspace.id} open={inviting} onOpenChange={setInviting} />
        </>
      ) : null}

      <RemoveDialog workspaceId={workspace.id} member={removing} onClose={() => setRemoving(null)} />
      {self ? <LeaveDialog workspace={workspace} open={leaving} onOpenChange={setLeaving} /> : null}
    </div>
  );
}

function PendingInvites({ workspaceId }: { workspaceId: string }) {
  const qc = useQueryClient();
  const invites = useQuery({ queryKey: ["invites", workspaceId], queryFn: () => api.invites(workspaceId) });
  const revoke = useMutation({
    mutationFn: api.revokeInvite,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["invites", workspaceId] }),
  });
  if (!invites.data || invites.data.length === 0) return null;
  return (
    <div className="flex flex-col gap-2">
      <h3 className="text-sm font-bold">Pending invites</h3>
      {revoke.error ? <Alert>{message(revoke.error, "Could not revoke the invite.")}</Alert> : null}
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Role</TableHead>
            <TableHead>Created by</TableHead>
            <TableHead>Expires</TableHead>
            <TableHead className="w-24" />
          </TableRow>
        </TableHeader>
        <TableBody>
          {invites.data.map((i) => (
            <TableRow key={i.id}>
              <TableCell>
                <Badge>{roleLabel(i.role)}</Badge>
              </TableCell>
              <TableCell className="text-muted-foreground">
                {i.createdByName ?? "Former user"}, {relativeTime(i.createdAt)}
              </TableCell>
              <TableCell className="text-muted-foreground">{expiresIn(i.expiresAt)}</TableCell>
              <TableCell className="text-right">
                <Button size="sm" variant="ghost" loading={revoke.isPending && revoke.variables === i.id} onClick={() => revoke.mutate(i.id)}>
                  Revoke
                </Button>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  );
}

function InviteDialog({ workspaceId, open, onOpenChange }: { workspaceId: string; open: boolean; onOpenChange: (o: boolean) => void }) {
  const qc = useQueryClient();
  const [role, setRole] = useState<Role>("member");
  const [days, setDays] = useState(7);
  const [url, setUrl] = useState<string | null>(null);
  const create = useMutation({
    mutationFn: () => api.createInvite(workspaceId, { role, expiresInDays: days }),
    onSuccess: async (r) => {
      setUrl(r.url);
      await qc.invalidateQueries({ queryKey: ["invites", workspaceId] });
    },
  });
  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) {
      setRole("member");
      setDays(7);
      setUrl(null);
      create.reset();
    }
  };
  return (
    <Modal
      open={open}
      onOpenChange={close}
      title={url ? "Share the invite link" : "Invite to workspace"}
      description={
        url
          ? "Send it to the person you're inviting. It works once and won't be shown again."
          : "Creates a single-use link. Anyone who opens it can join with this role."
      }
      width={480}
    >
      {url ? (
        <div className="flex flex-col gap-4">
          <div className="flex items-center gap-2 rounded-lg bg-muted p-3">
            <code className="min-w-0 flex-1 font-mono text-[13px] break-all select-all">{url}</code>
            <CopyButton value={url} label="Copy link" />
          </div>
          <div className="flex justify-end">
            <Button variant="primary" onClick={() => close(false)}>
              Done
            </Button>
          </div>
        </div>
      ) : (
        <form
          className="flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            create.mutate();
          }}
        >
          {create.error ? <Alert>{message(create.error, "Could not create the invite.")}</Alert> : null}
          <Field label="Role" hint={ROLE_HINTS[role]}>
            {(p) => (
              <Select {...p} value={role} onChange={(e) => setRole(e.target.value as Role)}>
                {ROLES.map((r) => (
                  <option key={r} value={r}>
                    {roleLabel(r)}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <Field label="Link expires after">
            {(p) => (
              <Select {...p} value={days} onChange={(e) => setDays(Number(e.target.value))}>
                {EXPIRY_CHOICES.map((d) => (
                  <option key={d} value={d}>
                    {d === 1 ? "1 day" : `${d} days`}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => close(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="primary" loading={create.isPending}>
              Create link
            </Button>
          </div>
        </form>
      )}
    </Modal>
  );
}

function RemoveDialog({ workspaceId, member, onClose }: { workspaceId: string; member: Member | null; onClose: () => void }) {
  const qc = useQueryClient();
  const remove = useMutation({
    mutationFn: (id: string) => api.removeMember(id),
    onSuccess: async () => {
      onClose();
      await qc.invalidateQueries({ queryKey: ["members", workspaceId] });
    },
  });
  return (
    <Modal
      open={member !== null}
      onOpenChange={(o) => {
        if (!o) {
          onClose();
          remove.reset();
        }
      }}
      title="Remove member"
      description={member ? `${member.name} (${member.email}) loses access to this workspace. API tokens limited to it are revoked.` : undefined}
    >
      <div className="flex flex-col gap-4">
        {remove.error ? <Alert>{message(remove.error, "Could not remove the member.")}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="danger" loading={remove.isPending} onClick={() => member && remove.mutate(member.id)}>
            Remove
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function LeaveDialog({ workspace, open, onOpenChange }: { workspace: Membership; open: boolean; onOpenChange: (o: boolean) => void }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const leave = useMutation({
    mutationFn: () => api.leaveWorkspace(workspace.id),
    onSuccess: async () => {
      onOpenChange(false);
      qc.removeQueries({ queryKey: ["members", workspace.id] });
      await qc.invalidateQueries({ queryKey: ["me"] });
      navigate({ to: "/projects" });
    },
  });
  return (
    <Modal
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) leave.reset();
      }}
      title={`Leave ${workspace.name}`}
      description="You lose access to its projects. An admin can invite you again."
    >
      <div className="flex flex-col gap-4">
        {leave.error ? <Alert>{message(leave.error, "Could not leave the workspace.")}</Alert> : null}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button variant="danger" loading={leave.isPending} onClick={() => leave.mutate()}>
            Leave workspace
          </Button>
        </div>
      </div>
    </Modal>
  );
}
