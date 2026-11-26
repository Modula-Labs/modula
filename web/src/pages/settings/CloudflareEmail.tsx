import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Input } from "@/components/ui/Input";
import { Alert, Skeleton, StatusDot } from "@/components/ui/misc";
import { cloudflare, emailReadinessMessage } from "@/lib/integrations";

export const cloudflareEmailKey = (ws: string) => ["cloudflare-email", ws];

/**
 * The address alert and report emails are sent from, through Cloudflare Email Service, and whether
 * Cloudflare will accept it. Shown once Cloudflare is connected (the API answers 409 before that).
 * Only admins can change it.
 */
export function CloudflareEmail({ workspaceId, canEdit }: { workspaceId: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const key = cloudflareEmailKey(workspaceId);
  // The readiness is checked live against Cloudflare: fetched on load and on "Check again", not on every focus.
  const settings = useQuery({
    queryKey: key,
    queryFn: () => cloudflare.email(workspaceId),
    staleTime: 60_000,
    refetchOnWindowFocus: false,
    retry: false,
  });
  // undefined until the field is edited; then the text being typed.
  const [typed, setTyped] = useState<string>();
  const saved = settings.data?.from ?? "";
  const value = typed ?? saved;
  const dirty = typed !== undefined && typed.trim().toLowerCase() !== saved;
  const save = useMutation({
    mutationFn: () => cloudflare.setEmail(workspaceId, value.trim() === "" ? null : value.trim()),
    onSuccess: (s) => {
      qc.setQueryData(key, s);
      setTyped(undefined);
    },
  });
  const readiness = settings.data && !dirty ? emailReadinessMessage(settings.data) : null;

  return (
    <div className="flex flex-col gap-2 pl-8">
      <div className="text-caps text-muted-foreground">Email</div>
      {settings.isPending ? (
        <Skeleton className="h-8 max-w-md" />
      ) : settings.error ? (
        <Alert tone="warn">Email: {settings.error.message}</Alert>
      ) : (
        <>
          <p className="text-sm text-muted-foreground">
            Email channels and the weekly report are sent from this address through Cloudflare Email Service. Its domain must be onboarded under Email Service →
            Email Sending, and the token needs Email Sending · Edit.
          </p>
          <form
            className="flex flex-wrap items-center gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              if (dirty) save.mutate();
            }}
          >
            <Input
              type="email"
              aria-label="Sender address"
              autoComplete="off"
              className="max-w-xs min-w-[14rem] flex-1"
              placeholder="alerts@example.com"
              value={value}
              disabled={!canEdit}
              onChange={(e) => setTyped(e.target.value)}
            />
            {canEdit ? (
              <Button type="submit" loading={save.isPending} disabled={!dirty}>
                Save
              </Button>
            ) : null}
          </form>
          {canEdit ? <p className="text-sm text-muted-foreground">Leave it empty to stop email channels from sending.</p> : null}
          {save.error ? <Alert>{save.error.message}</Alert> : null}
          {readiness ? (
            <div className="flex flex-wrap items-center gap-2">
              <StatusDot tone={readiness.tone}>{readiness.text}</StatusDot>
              {readiness.tone === "warn" ? (
                <Button variant="ghost" size="sm" loading={settings.isFetching} onClick={() => void settings.refetch()}>
                  Check again
                </Button>
              ) : null}
            </div>
          ) : null}
        </>
      )}
    </div>
  );
}
