import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Plus, Terminal } from "@/components/ui/icons";
import { Alert, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type ApiToken } from "@/lib/api";
import { relativeTime } from "@/lib/format";

const EXPIRY_CHOICES = [
  { label: "30 days", days: 30 },
  { label: "90 days", days: 90 },
  { label: "1 year", days: 365 },
  { label: "Never", days: 0 },
];

function scopeLabel(t: ApiToken) {
  return t.workspaceName ?? (t.workspaceId ? "One workspace" : "All workspaces");
}

function CreateDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  const qc = useQueryClient();
  const me = useQuery({ queryKey: ["me"], queryFn: api.me, enabled: open });
  const [name, setName] = useState("");
  const [workspace, setWorkspace] = useState("");
  const [days, setDays] = useState(90);
  const [created, setCreated] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setName("");
    setWorkspace("");
    setDays(90);
    setCreated(null);
    setError(null);
  };
  const create = useMutation({
    mutationFn: () =>
      api.createApiToken({
        name: name.trim(),
        workspaceId: workspace || undefined,
        expiresInDays: days || undefined,
      }),
    onSuccess: async (r) => {
      setCreated(r.token);
      await qc.invalidateQueries({ queryKey: ["api-tokens"] });
    },
    onError: (e) => setError(e instanceof ApiError ? e.message : "Could not create the token."),
  });

  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) reset();
  };

  return (
    <Modal
      open={open}
      onOpenChange={close}
      title={created ? "Copy your token" : "New API token"}
      description={
        created
          ? "It won't be shown again. Store it like a password."
          : "Tokens act as you (never with re-authentication), for the CLI and automation."
      }
      width={480}
    >
      {created ? (
        <div className="flex flex-col gap-4">
          <div className="flex items-center gap-2 rounded-md bg-accent p-3">
            <code className="min-w-0 flex-1 font-mono text-[13px] break-all select-all">{created}</code>
            <CopyButton value={created} label="Copy token" />
          </div>
          <div className="text-sm text-muted-foreground">
            Use it with the CLI:
            <code className="mt-1 block rounded-md bg-accent p-2 font-mono text-[13px]">
              modula-deploy login --url {window.location.origin}
            </code>
          </div>
          <div className="flex justify-end">
            <Button variant="primary" onClick={() => close(false)}>Done</Button>
          </div>
        </div>
      ) : (
        <form
          className="flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            setError(null);
            create.mutate();
          }}
        >
          {error ? <Alert>{error}</Alert> : null}
          <Field label="Name" hint="Where it's used, e.g. “laptop” or “GitHub Actions”.">
            {(p) => <Input {...p} autoFocus maxLength={64} value={name} onChange={(e) => setName(e.target.value)} />}
          </Field>
          <Field label="Access">
            {(p) => (
              <Select {...p} value={workspace} onChange={(e) => setWorkspace(e.target.value)}>
                <option value="">All my workspaces</option>
                {me.data?.workspaces.map((w) => (
                  <option key={w.id} value={w.id}>Only {w.name}</option>
                ))}
              </Select>
            )}
          </Field>
          <Field label="Expires">
            {(p) => (
              <Select {...p} value={days} onChange={(e) => setDays(Number(e.target.value))}>
                {EXPIRY_CHOICES.map((c) => (
                  <option key={c.days} value={c.days}>{c.label}</option>
                ))}
              </Select>
            )}
          </Field>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => close(false)}>Cancel</Button>
            <Button type="submit" variant="primary" loading={create.isPending} disabled={!name.trim()}>
              Create token
            </Button>
          </div>
        </form>
      )}
    </Modal>
  );
}

/** The API tokens list on the account page: create (shown once), revoke. */
export function ApiTokens() {
  const qc = useQueryClient();
  const tokens = useQuery({ queryKey: ["api-tokens"], queryFn: api.apiTokens });
  const [creating, setCreating] = useState(false);
  const revoke = useMutation({
    mutationFn: api.revokeApiToken,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["api-tokens"] }),
  });

  return (
    <div className="flex flex-col gap-3">
      {tokens.isLoading ? (
        <Skeleton className="h-16" />
      ) : tokens.data && tokens.data.length > 0 ? (
        <div className="overflow-hidden rounded-lg border border-border">
          <table className="w-full text-sm">
            <thead className="bg-card text-left text-xs text-muted-foreground">
              <tr>
                <th className="px-4 py-2 font-medium">Name</th>
                <th className="px-4 py-2 font-medium">Access</th>
                <th className="px-4 py-2 font-medium">Last used</th>
                <th className="px-4 py-2 font-medium">Expires</th>
                <th className="w-24" />
              </tr>
            </thead>
            <tbody>
              {tokens.data.map((t) => (
                <tr key={t.id} className="border-t border-border">
                  <td className="px-4 py-3">
                    <span className="flex items-center gap-2">
                      <Terminal className="size-4 text-muted-foreground" />
                      {t.name}
                    </span>
                  </td>
                  <td className="px-4 py-3 text-muted-foreground">{scopeLabel(t)}</td>
                  <td className="px-4 py-3 text-muted-foreground">{t.lastUsedAt ? relativeTime(t.lastUsedAt) : "Never"}</td>
                  <td className="px-4 py-3 text-muted-foreground">
                    {t.expiresAt ? new Date(t.expiresAt).toLocaleDateString() : "Never"}
                  </td>
                  <td className="px-4 py-3 text-right">
                    <Button
                      size="sm"
                      variant="ghost"
                      loading={revoke.isPending && revoke.variables === t.id}
                      onClick={() => revoke.mutate(t.id)}
                    >
                      Revoke
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <p className="text-sm text-muted-foreground">No tokens yet.</p>
      )}
      <div>
        <Button variant="outline" icon={<Plus className="size-3.5" />} onClick={() => setCreating(true)}>
          New token
        </Button>
      </div>
      <CreateDialog open={creating} onOpenChange={setCreating} />
    </div>
  );
}
