import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { AuthCard } from "@/components/AuthCard";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { ApiError, api } from "@/lib/api";

/** `invite`: the invite token to return to after signing in. */
export function LoginPage({ notice, invite }: { notice?: string; invite?: string }) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  return (
    <AuthCard title="Sign in" subtitle={invite ? "Sign in to accept your invite." : "Welcome back."}>
      <form
        className="flex flex-col gap-4"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            const r = await api.login({ email, password });
            if (r.mfaRequired) {
              await navigate({ to: "/login/2fa", search: invite ? { invite } : {} });
            } else {
              qc.setQueryData(["me"], r.me);
              await (invite ? navigate({ to: "/invite/$token", params: { token: invite } }) : navigate({ to: "/projects" }));
            }
          } catch (err) {
            const ae = err instanceof ApiError ? err : null;
            setError(
              ae?.code === "rate_limited"
                ? "Too many attempts. Wait a few minutes and try again."
                : ae?.code === "invalid_credentials"
                  ? "Incorrect email or password."
                  : (ae?.message ?? "Something went wrong."),
            );
            setPassword("");
          } finally {
            setBusy(false);
          }
        }}
      >
        {notice && !error ? <Alert tone="info">{notice}</Alert> : null}
        {error ? <Alert>{error}</Alert> : null}
        <Field label="Email">
          {(p) => <Input {...p} type="email" required autoFocus autoComplete="username" value={email} onChange={(e) => setEmail(e.target.value)} />}
        </Field>
        <Field label="Password">
          {(p) => <Input {...p} type="password" required autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} />}
        </Field>
        <Button type="submit" variant="primary" loading={busy}>
          Sign in
        </Button>
      </form>
    </AuthCard>
  );
}
