import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { AuthCard } from "@/components/AuthCard";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { OtpInput } from "@/components/ui/OtpInput";
import { Key } from "@/components/ui/icons";
import { Alert } from "@/components/ui/misc";
import { ApiError, api, type Me } from "@/lib/api";
import { getPasskey, passkeyError, passkeysSupported } from "@/lib/webauthn";
import { type ReturnTo, goBack } from "./Login";

type Mode = "passkey" | "totp" | "recovery";

const SUBTITLE: Record<Mode, string> = {
  passkey: "Use the passkey saved on your device or security key.",
  totp: "Enter the 6-digit code from your authenticator app.",
  recovery: "Enter one of your recovery codes. Each code works once.",
};

/** `back`: where to return after the second factor. */
export function TwoFactorPage({ back }: { back: ReturnTo }) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const methods = useQuery({ queryKey: ["mfa-methods"], queryFn: api.mfaMethods, retry: false, gcTime: 0 });
  const passkey = !!methods.data?.passkey && passkeysSupported();
  const totp = !!methods.data?.totp;
  const [mode, setMode] = useState<Mode | null>(null);
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [expired, setExpired] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (methods.data && !mode) setMode(passkey ? "passkey" : totp ? "totp" : "recovery");
    if (methods.isError) {
      setExpired(true);
      setError("This sign-in expired. Sign in again.");
    }
  }, [methods.data, methods.isError, mode, passkey, totp]);

  const verify = async (send: () => Promise<Me>, failMsg: string) => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const me = await send();
      qc.setQueryData(["me"], me);
      await goBack(navigate, back);
    } catch (err) {
      const ae = err instanceof ApiError ? err : null;
      if (ae?.code === "rate_limited" || ae?.code === "unauthorized") {
        setExpired(true);
        setError(ae.code === "rate_limited" ? "Too many attempts. Sign in again to get a new challenge." : "This sign-in expired. Sign in again.");
      } else {
        setError(ae ? failMsg : passkeyError(err));
      }
      setCode("");
    } finally {
      setBusy(false);
    }
  };

  const submit = (value = code) =>
    mode === "recovery"
      ? verify(() => api.mfa({ backupCode: value }), "That recovery code didn't work.")
      : verify(() => api.mfa({ totpCode: value }), "That code didn't work. Wait for a new one and try again.");

  const usePasskey = () =>
    verify(async () => {
      const options = await api.mfaPasskeyOptions();
      return api.mfa({ passkey: await getPasskey(options) });
    }, "That passkey didn't work.");

  const switchTo = (m: Mode) => {
    setMode(m);
    setCode("");
    setError(null);
  };

  const current = mode ?? "totp";
  const others: { mode: Mode; label: string }[] = [
    ...(passkey && current !== "passkey" ? [{ mode: "passkey" as const, label: "Use a passkey" }] : []),
    ...(totp && current !== "totp" ? [{ mode: "totp" as const, label: "Use your authenticator app" }] : []),
  ];

  return (
    <AuthCard
      title="Two-factor authentication"
      subtitle={SUBTITLE[current]}
      footer={<Link to="/login" search={back} className="hover:text-foreground">← Back to sign in</Link>}
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (current === "passkey") void usePasskey();
          else void submit();
        }}
      >
        {error ? <Alert>{error}</Alert> : null}
        {!mode && !expired ? null : current === "passkey" ? null : current === "recovery" ? (
          <Field label="Recovery code">
            {(p) => <Input {...p} mono autoFocus autoComplete="off" spellCheck={false} placeholder="xxxxx-xxxxx" value={code} disabled={expired} onChange={(e) => setCode(e.target.value)} />}
          </Field>
        ) : (
          <OtpInput value={code} onChange={setCode} onComplete={(v) => void submit(v)} autoFocus disabled={busy || expired} invalid={!!error} />
        )}
        {expired ? (
          <Button type="button" variant="primary" onClick={() => void navigate({ to: "/login", search: back })}>
            Sign in again
          </Button>
        ) : current === "passkey" ? (
          <Button type="submit" variant="primary" icon={<Key />} loading={busy} autoFocus>
            Use passkey
          </Button>
        ) : (
          <Button type="submit" variant="primary" loading={busy} disabled={!mode || (current === "recovery" ? code.length < 10 : code.length !== 6)}>
            Verify
          </Button>
        )}
        <div className="flex flex-col items-start gap-1.5">
          {others.map((o) => (
            <button key={o.mode} type="button" className="cursor-pointer text-sm text-foreground underline-offset-4 hover:underline" onClick={() => switchTo(o.mode)}>
              {o.label}
            </button>
          ))}
          {current !== "recovery" ? (
            <button type="button" className="cursor-pointer text-sm text-muted-foreground underline-offset-4 hover:text-foreground hover:underline" onClick={() => switchTo("recovery")}>
              Lost your 2FA device? Use a recovery code
            </button>
          ) : null}
        </div>
      </form>
    </AuthCard>
  );
}
