import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { AuthCard } from "@/components/AuthCard";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { OtpInput } from "@/components/ui/OtpInput";
import { Alert } from "@/components/ui/misc";
import { ApiError, api } from "@/lib/api";

/** `invite`: the invite token to return to after the second factor. */
export function TwoFactorPage({ invite }: { invite?: string }) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [recovery, setRecovery] = useState(false);
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [expired, setExpired] = useState(false);
  const [busy, setBusy] = useState(false);

  const submit = async (value = code) => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const me = await api.mfa(recovery ? { backupCode: value } : { totpCode: value });
      qc.setQueryData(["me"], me);
      await (invite ? navigate({ to: "/invite/$token", params: { token: invite } }) : navigate({ to: "/projects" }));
    } catch (err) {
      const ae = err instanceof ApiError ? err : null;
      if (ae?.code === "rate_limited" || ae?.code === "unauthorized") {
        setExpired(true);
        setError(ae.code === "rate_limited" ? "Too many attempts. Sign in again to get a new challenge." : "This sign-in expired. Sign in again.");
      } else {
        setError(recovery ? "That recovery code didn't work." : "That code didn't work. Wait for a new one and try again.");
      }
      setCode("");
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthCard
      title="Two-factor authentication"
      subtitle={recovery ? "Enter one of your recovery codes. Each code works once." : "Enter the 6-digit code from your authenticator app."}
      footer={<Link to="/login" search={invite ? { invite } : {}} className="hover:text-foreground">← Back to sign in</Link>}
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        {error ? <Alert>{error}</Alert> : null}
        {recovery ? (
          <Field label="Recovery code">
            {(p) => <Input {...p} mono autoFocus autoComplete="off" spellCheck={false} placeholder="xxxxx-xxxxx" value={code} disabled={expired} onChange={(e) => setCode(e.target.value)} />}
          </Field>
        ) : (
          <OtpInput value={code} onChange={setCode} onComplete={(v) => void submit(v)} autoFocus disabled={busy || expired} invalid={!!error} />
        )}
        {expired ? (
          <Button type="button" variant="primary" onClick={() => void navigate({ to: "/login", search: invite ? { invite } : {} })}>
            Sign in again
          </Button>
        ) : (
          <Button type="submit" variant="primary" loading={busy} disabled={recovery ? code.length < 10 : code.length !== 6}>
            Verify
          </Button>
        )}
        <button
          type="button"
          className="cursor-pointer self-start text-sm text-foreground underline-offset-4 hover:underline"
          onClick={() => {
            setRecovery(!recovery);
            setCode("");
            setError(null);
          }}
        >
          {recovery ? "Use your authenticator app" : "Use a recovery code"}
        </button>
      </form>
    </AuthCard>
  );
}
