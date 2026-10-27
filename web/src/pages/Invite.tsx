import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { AuthCard } from "@/components/AuthCard";
import { StrengthMeter } from "@/components/StrengthMeter";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type InviteJoined, type InvitePreview } from "@/lib/api";
import { expiresIn, roleLabel } from "@/lib/members";
import { strength } from "@/lib/password";
import { save } from "@/lib/storage";

function errorText(e: unknown): string {
  const ae = e instanceof ApiError ? e : null;
  if (ae?.code === "rate_limited") return "Too many attempts. Wait a few minutes and try again.";
  return ae?.message ?? "Something went wrong.";
}

/** Public page an invite link opens: join as the signed-in user, or create an account. */
export function InvitePage({ token }: { token: string }) {
  const preview = useQuery({
    queryKey: ["invite", token],
    queryFn: () => api.lookupInvite(token),
    retry: false,
    staleTime: Infinity,
  });
  // Signed out is a normal state here, so a 401 is not retried or surfaced.
  const me = useQuery({ queryKey: ["me"], queryFn: api.me, retry: false });

  if (preview.isLoading || me.isLoading) {
    return (
      <AuthCard title="Invitation">
        <Skeleton className="h-24" />
      </AuthCard>
    );
  }
  if (preview.error || !preview.data) {
    const notFound = preview.error instanceof ApiError && preview.error.status === 404;
    return (
      <AuthCard
        title="This invite can't be used"
        subtitle={notFound ? "The link is invalid, expired, revoked or already used. Ask a workspace admin for a new one." : errorText(preview.error)}
        footer={
          <Link to="/login" className="hover:text-foreground">
            Sign in
          </Link>
        }
      >
        {null}
      </AuthCard>
    );
  }
  const signedIn = !!me.data;
  return (
    <AuthCard title={`Join ${preview.data.workspaceName}`} subtitle={<Summary invite={preview.data} />}>
      {signedIn ? <Accept token={token} invite={preview.data} email={me.data.user.email} /> : <Signup token={token} />}
    </AuthCard>
  );
}

function Summary({ invite }: { invite: InvitePreview }) {
  return (
    <span className="flex flex-col gap-2">
      <span>
        {invite.invitedBy ?? "Someone"} invited you to the {invite.workspaceName} workspace.
      </span>
      <span className="flex items-center gap-2">
        Role <Badge>{roleLabel(invite.role)}</Badge>
        <span className="text-[13px]">· Link expires {expiresIn(invite.expiresAt)}</span>
      </span>
    </span>
  );
}

function useJoined() {
  const qc = useQueryClient();
  const navigate = useNavigate();
  return async (r: InviteJoined) => {
    save("mc.workspace", r.workspaceId);
    qc.setQueryData(["me"], r.me);
    await navigate({ to: "/projects" });
  };
}

function Accept({ token, invite, email }: { token: string; invite: InvitePreview; email: string }) {
  const joined = useJoined();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  return (
    <div className="flex flex-col gap-4">
      {error ? <Alert>{error}</Alert> : null}
      <p className="text-sm text-muted-foreground">Signed in as {email}.</p>
      <Button
        variant="primary"
        loading={busy}
        onClick={async () => {
          setBusy(true);
          setError(null);
          try {
            await joined(await api.acceptInvite(token));
          } catch (e) {
            setError(e instanceof ApiError && e.status === 409 ? `You're already a member of ${invite.workspaceName}.` : errorText(e));
          } finally {
            setBusy(false);
          }
        }}
      >
        Join {invite.workspaceName}
      </Button>
    </div>
  );
}

function Signup({ token }: { token: string }) {
  const joined = useJoined();
  const [form, setForm] = useState({ name: "", email: "", password: "" });
  const [error, setError] = useState<string | null>(null);
  const [exists, setExists] = useState(false);
  const [busy, setBusy] = useState(false);
  const s = strength(form.password);
  const set = (k: keyof typeof form) => (e: React.ChangeEvent<HTMLInputElement>) => setForm({ ...form, [k]: e.target.value });
  return (
    <form
      className="flex flex-col gap-4"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setError(null);
        setExists(false);
        try {
          await joined(await api.signupWithInvite({ token, ...form }));
        } catch (err) {
          if (err instanceof ApiError && err.status === 409) {
            setExists(true);
          } else {
            setError(errorText(err));
          }
        } finally {
          setBusy(false);
        }
      }}
    >
      {exists ? (
        <Alert tone="info">
          An account with this email already exists.{" "}
          <Link to="/login" search={{ invite: token }} className="underline">
            Sign in to accept
          </Link>
          .
        </Alert>
      ) : null}
      {error ? <Alert>{error}</Alert> : null}
      <Field label="Name">{(p) => <Input {...p} required autoFocus autoComplete="name" value={form.name} onChange={set("name")} />}</Field>
      <Field label="Email">{(p) => <Input {...p} type="email" required autoComplete="email" value={form.email} onChange={set("email")} />}</Field>
      <Field label="Password" hint={s.label}>
        {(p) => <Input {...p} type="password" required minLength={10} autoComplete="new-password" value={form.password} onChange={set("password")} />}
      </Field>
      <StrengthMeter score={s.score} />
      <Button type="submit" variant="primary" loading={busy} disabled={s.score === 0}>
        Create account and join
      </Button>
      <p className="text-center text-sm text-muted-foreground">
        Already have an account?{" "}
        <Link to="/login" search={{ invite: token }} className="text-foreground underline-offset-4 hover:underline">
          Sign in
        </Link>
      </p>
    </form>
  );
}
