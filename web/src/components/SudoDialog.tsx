import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { ApiError, api, setSudoHandler, type Security } from "@/lib/api";
import { Button } from "./ui/Button";
import { Field, Input } from "./ui/Input";
import { Modal } from "./ui/Modal";
import { OtpInput } from "./ui/OtpInput";
import { Alert } from "./ui/misc";

/** "Confirm it's you": opened by the API client when the server answers
 *  `sudo_required`; resolves true once re-authenticated so the request retries. */
export function SudoDialog() {
  const qc = useQueryClient();
  const [open, setOpen] = useState(false);
  const [totp, setTotp] = useState(false);
  const [recovery, setRecovery] = useState(false);
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const resolver = useRef<((ok: boolean) => void) | null>(null);

  useEffect(() => {
    setSudoHandler(async () => {
      const sec = await qc.fetchQuery<Security>({ queryKey: ["security"], queryFn: api.security, staleTime: 0 });
      setTotp(sec.totpEnabled);
      setRecovery(false);
      setValue("");
      setError(null);
      setOpen(true);
      return new Promise<boolean>((resolve) => {
        resolver.current = resolve;
      });
    });
    return () => setSudoHandler(null);
  }, [qc]);

  const finish = (ok: boolean) => {
    resolver.current?.(ok);
    resolver.current = null;
    setOpen(false);
  };

  const submit = async (v = value) => {
    setBusy(true);
    setError(null);
    try {
      await api.sudo(!totp ? { password: v } : recovery ? { backupCode: v } : { totpCode: v });
      await qc.invalidateQueries({ queryKey: ["security"] });
      finish(true);
    } catch (e) {
      const code = e instanceof ApiError ? e.code : "";
      setError(code === "rate_limited" ? "Too many attempts. Try again later." : "That didn't work. Try again.");
      setValue("");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onOpenChange={(o) => !o && finish(false)}
      title="Confirm it's you"
      description="This action needs a recent sign-in. It stays unlocked for 15 minutes."
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        {error ? <Alert>{error}</Alert> : null}
        {!totp ? (
          <Field label="Password">
            {(p) => (
              <Input {...p} type="password" autoFocus autoComplete="current-password" value={value} onChange={(e) => setValue(e.target.value)} />
            )}
          </Field>
        ) : recovery ? (
          <Field label="Recovery code">
            {(p) => <Input {...p} mono autoFocus placeholder="xxxxx-xxxxx" value={value} onChange={(e) => setValue(e.target.value)} />}
          </Field>
        ) : (
          <div className="flex flex-col gap-2">
            <span className="text-sm font-medium">Authenticator code</span>
            <OtpInput value={value} onChange={setValue} onComplete={(v) => void submit(v)} autoFocus disabled={busy} invalid={!!error} />
          </div>
        )}
        <div className="flex items-center justify-between">
          {totp ? (
            <button type="button" className="text-sm text-brand-text hover:underline" onClick={() => { setRecovery(!recovery); setValue(""); }}>
              {recovery ? "Use authenticator app" : "Use a recovery code"}
            </button>
          ) : <span />}
          <div className="flex gap-2">
            <Button type="button" variant="ghost" onClick={() => finish(false)}>Cancel</Button>
            <Button type="submit" variant="primary" loading={busy} disabled={!value}>Confirm</Button>
          </div>
        </div>
      </form>
    </Modal>
  );
}
