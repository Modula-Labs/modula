import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { ApiError, api, setSudoHandler, type Security } from "@/lib/api";
import { getPasskey, passkeyError, passkeysSupported, rpMatchesHere } from "@/lib/webauthn";
import { Button } from "./ui/Button";
import { Field, Input } from "./ui/Input";
import { Modal } from "./ui/Modal";
import { OtpInput } from "./ui/OtpInput";
import { Key } from "./ui/icons";
import { Alert } from "./ui/misc";

type Mode = "password" | "passkey" | "totp" | "recovery";

/** "Confirm it's you": opened by the API client when the server answers
 *  `sudo_required`; resolves true once re-authenticated so the request retries.
 *  With 2FA on, a second factor is required (the server rejects a password). */
export function SudoDialog() {
  const qc = useQueryClient();
  const [open, setOpen] = useState(false);
  const [methods, setMethods] = useState({ passkey: false, totp: false });
  const [mode, setMode] = useState<Mode>("password");
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const resolver = useRef<((ok: boolean) => void) | null>(null);

  useEffect(() => {
    setSudoHandler(async () => {
      const sec = await qc.fetchQuery<Security>({ queryKey: ["security"], queryFn: api.security, staleTime: 0 });
      const passkey =
        passkeysSupported() &&
        rpMatchesHere(sec.passkeyRpId) &&
        sec.passkeys.some((p) => p.rpId === sec.passkeyRpId);
      const twoFactor = sec.totpEnabled || sec.passkeys.length > 0;
      setMethods({ passkey, totp: sec.totpEnabled });
      setMode(!twoFactor ? "password" : passkey ? "passkey" : sec.totpEnabled ? "totp" : "recovery");
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
      if (mode === "passkey") {
        const options = await api.sudoPasskeyOptions();
        await api.sudo({ passkey: await getPasskey(options) });
      } else {
        await api.sudo(mode === "password" ? { password: v } : mode === "recovery" ? { backupCode: v } : { totpCode: v });
      }
      await qc.invalidateQueries({ queryKey: ["security"] });
      finish(true);
    } catch (e) {
      const code = e instanceof ApiError ? e.code : "";
      setError(
        code === "rate_limited"
          ? "Too many attempts. Try again later."
          : mode === "passkey" && !(e instanceof ApiError)
            ? passkeyError(e)
            : "That didn't work. Try again.",
      );
      setValue("");
    } finally {
      setBusy(false);
    }
  };

  const switchTo = (m: Mode) => {
    setMode(m);
    setValue("");
    setError(null);
  };
  const link = "cursor-pointer text-left text-sm text-brand-text hover:underline";

  return (
    <Modal
      open={open}
      onOpenChange={(o) => !o && finish(false)}
      title="Confirm it's you"
      description="Confirm once a day to unlock sensitive actions on this device."
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        {error ? <Alert>{error}</Alert> : null}
        {mode === "password" ? (
          <Field label="Password">
            {(p) => (
              <Input {...p} type="password" autoFocus autoComplete="current-password" value={value} onChange={(e) => setValue(e.target.value)} />
            )}
          </Field>
        ) : mode === "passkey" ? (
          <p className="text-sm text-muted-foreground">Use the passkey saved on your device or security key.</p>
        ) : mode === "recovery" ? (
          <Field label="Recovery code" hint="Each code works once.">
            {(p) => <Input {...p} mono autoFocus placeholder="xxxxx-xxxxx" value={value} onChange={(e) => setValue(e.target.value)} />}
          </Field>
        ) : (
          <div className="flex flex-col gap-2">
            <span className="text-sm font-normal">Authenticator code</span>
            <OtpInput value={value} onChange={setValue} onComplete={(v) => void submit(v)} autoFocus disabled={busy} invalid={!!error} />
          </div>
        )}
        {mode !== "password" ? (
          <div className="flex flex-col items-start gap-1.5">
            {methods.passkey && mode !== "passkey" ? (
              <button type="button" className={link} onClick={() => switchTo("passkey")}>Use a passkey</button>
            ) : null}
            {methods.totp && mode !== "totp" ? (
              <button type="button" className={link} onClick={() => switchTo("totp")}>Use your authenticator app</button>
            ) : null}
            {mode !== "recovery" ? (
              <button type="button" className={link} onClick={() => switchTo("recovery")}>
                Lost your 2FA device? Use a recovery code
              </button>
            ) : null}
          </div>
        ) : null}
        <div className="flex justify-end gap-2 pt-2">
          <Button type="button" variant="outline" onClick={() => finish(false)}>Cancel</Button>
          {mode === "passkey" ? (
            <Button type="submit" variant="primary" icon={<Key />} loading={busy}>Use passkey</Button>
          ) : (
            <Button type="submit" variant="primary" loading={busy} disabled={!value}>Confirm</Button>
          )}
        </div>
      </form>
    </Modal>
  );
}
