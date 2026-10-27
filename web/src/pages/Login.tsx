import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { AuthCard } from "@/components/AuthCard";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { ApiError, api } from "@/lib/api";

/** Where to return after signing in: an invite to accept, or a CLI login to approve. */
export interface ReturnTo {
  invite?: string;
  cli?: string;
}

/** After a full sign-in: back to `back`, else the projects. */
export function goBack(navigate: ReturnType<typeof useNavigate>, back: ReturnTo) {
  if (back.invite) return navigate({ to: "/invite/$token", params: { token: back.invite } });
  if (back.cli) return navigate({ to: "/cli/login", search: { code: back.cli } });
  return navigate({ to: "/projects" });
}

/** `back`: where to return after signing in. */
export function LoginPage({ notice, back }: { notice?: string; back: ReturnTo }) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  return (
    <AuthCard title="Sign in" subtitle={back.invite ? "Sign in to accept your invite." : back.cli ? "Sign in to approve the CLI login." : "Welcome back."}>
      <form
        className="flex flex-col gap-4"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            const r = await api.login({ email, password });
            if (r.mfaRequired) {
              await navigate({ to: "/login/2fa", search: back });
            } else {
              qc.setQueryData(["me"], r.me);
              await goBack(navigate, back);
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
