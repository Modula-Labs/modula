import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Copy, Download, FileText, Key, Phone, World } from "@/components/ui/icons";
import QRCode from "qrcode";
import { useEffect, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { OtpInput } from "@/components/ui/OtpInput";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { ApiError, api } from "@/lib/api";
import { describeAgent, relativeTime } from "@/lib/format";
import { ApiTokens } from "./AccountApiTokens";

function Section({ title, description, children }: { title: string; description: string; children: ReactNode }) {
  return (
    <section className="border-t border-border py-8 first:border-t-0 first:pt-0">
      <h2 className="text-lg font-semibold">{title}</h2>
      <p className="mt-1 text-sm text-muted-foreground">{description}</p>
      <div className="mt-5">{children}</div>
    </section>
  );
}

function Row({ icon, title, meta, action }: { icon: ReactNode; title: string; meta: ReactNode; action: ReactNode }) {
  return (
    <div className="flex items-center gap-4 rounded-lg border border-border bg-card px-4 py-3">
      <span className="text-muted-foreground [&>svg]:size-5">{icon}</span>
      <div className="min-w-0 flex-1">
        <div className="text-sm font-medium">{title}</div>
        <div className="text-xs text-muted-foreground">{meta}</div>
      </div>
      {action}
    </div>
  );
}

export function RecoveryCodes({ codes }: { codes: string[] }) {
  const text = codes.join("\n");
  return (
    <div className="flex flex-col gap-3">
      <div className="grid grid-cols-2 gap-2 rounded-md bg-accent p-4 font-mono text-[13px]">
        {codes.map((c) => (
          <span key={c}>{c}</span>
        ))}
      </div>
      <div className="flex gap-2">
        <Button size="sm" icon={<Copy className="size-3.5" />} onClick={() => void navigator.clipboard?.writeText(text)}>
          Copy
        </Button>
        <Button
          size="sm"
          icon={<Download className="size-3.5" />}
          onClick={() => {
            const url = URL.createObjectURL(new Blob([text + "\n"], { type: "text/plain" }));
            const a = document.createElement("a");
            a.href = url;
            a.download = "modula-deploy-recovery-codes.txt";
            a.click();
            URL.revokeObjectURL(url);
          }}
        >
          Download
        </Button>
      </div>
    </div>
  );
}

function EnrollDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  const qc = useQueryClient();
  const [step, setStep] = useState<"scan" | "codes">("scan");
  const [enrollment, setEnrollment] = useState<{ otpauthUrl: string; secret: string } | null>(null);
  const [qr, setQr] = useState<string | null>(null);
  const [code, setCode] = useState("");
  const [codes, setCodes] = useState<string[]>([]);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setStep("scan");
    setCode("");
    setSaved(false);
    setError(null);
    setEnrollment(null);
    api
      .totpBegin()
      .then(async (e) => {
        setEnrollment(e);
        setQr(await QRCode.toDataURL(e.otpauthUrl, { margin: 1, width: 176, errorCorrectionLevel: "M" }));
      })
      .catch((e) => {
        setError(e instanceof ApiError && e.code === "sudo_required" ? "Confirm it's you to continue." : (e as Error).message);
      });
  }, [open]);

  const confirm = async (value = code) => {
    setBusy(true);
    setError(null);
    try {
      const r = await api.totpConfirm(value);
      setCodes(r.backupCodes);
      setStep("codes");
      await qc.invalidateQueries({ queryKey: ["security"] });
    } catch (e) {
      setError(e instanceof ApiError ? e.message : "Could not verify the code.");
      setCode("");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onOpenChange={(o) => (step === "codes" && !saved ? undefined : onOpenChange(o))}
      title={step === "scan" ? "Set up authenticator app" : "Save your recovery codes"}
      description={
        step === "scan"
          ? "Scan the QR code with an authenticator app, then enter the 6-digit code it shows."
          : "Each code signs you in once if you lose your device. They won't be shown again."
      }
      width={480}
    >
      {error ? <div className="mb-4"><Alert>{error}</Alert></div> : null}
      {step === "scan" ? (
        <div className="flex flex-col gap-5">
          <div className="flex items-start gap-5">
            <div className="flex size-[176px] shrink-0 items-center justify-center rounded-md bg-white p-1">
              {qr ? <img src={qr} alt="QR code for your authenticator app" width={168} height={168} /> : <Skeleton className="size-full" />}
            </div>
            <div className="min-w-0 text-sm">
              <div className="text-muted-foreground">Can't scan? Enter this key manually:</div>
              <code className="mt-2 block rounded-md bg-accent p-2 font-mono text-[13px] break-all select-all">
                {enrollment?.secret.match(/.{1,4}/g)?.join(" ") ?? "…"}
              </code>
            </div>
          </div>
          <div className="flex flex-col gap-2">
            <span className="text-sm font-medium">Code from the app</span>
            <OtpInput value={code} onChange={setCode} onComplete={(v) => void confirm(v)} disabled={busy || !enrollment} invalid={!!error} />
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={() => onOpenChange(false)}>Cancel</Button>
            <Button variant="primary" loading={busy} disabled={code.length !== 6} onClick={() => void confirm()}>Verify</Button>
          </div>
        </div>
      ) : (
        <div className="flex flex-col gap-5">
          <RecoveryCodes codes={codes} />
          <label className="flex items-center gap-2 text-sm">
            <input type="checkbox" checked={saved} onChange={(e) => setSaved(e.target.checked)} className="size-4 accent-[hsl(var(--accent))]" />
            I saved these codes somewhere safe
          </label>
          <div className="flex justify-end">
            <Button variant="primary" disabled={!saved} onClick={() => onOpenChange(false)}>Done</Button>
          </div>
        </div>
      )}
    </Modal>
  );
}

export function AccountSecurityPage() {
  const qc = useQueryClient();
  const security = useQuery({ queryKey: ["security"], queryFn: api.security });
  const sessions = useQuery({ queryKey: ["sessions"], queryFn: api.sessions });
  const [enroll, setEnroll] = useState(false);
  const [disable, setDisable] = useState(false);
  const [newCodes, setNewCodes] = useState<string[] | null>(null);
  const [pw, setPw] = useState("");
  const [revokeTokens, setRevokeTokens] = useState(true);
  const [pwMsg, setPwMsg] = useState<{ tone: "success" | "danger"; text: string } | null>(null);

  const disableMut = useMutation({
    mutationFn: api.totpDisable,
    onSuccess: async () => {
      setDisable(false);
      await qc.invalidateQueries({ queryKey: ["security"] });
    },
  });
  const regen = useMutation({ mutationFn: api.backupCodes, onSuccess: (r) => setNewCodes(r.backupCodes) });
  const revoke = useMutation({
    mutationFn: api.revokeSession,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["sessions"] }),
  });
  const changePw = useMutation({
    mutationFn: (v: { pw: string; keepTokens: boolean }) => api.changePassword(v.pw, v.keepTokens),
    onSuccess: async (r) => {
      setPw("");
      const tokens = r.tokensRevoked ? ` ${r.tokensRevoked} API token(s) revoked.` : "";
      setPwMsg({ tone: "success", text: `Password changed. ${r.sessionsRevoked} other session(s) signed out.${tokens}` });
      await qc.invalidateQueries({ queryKey: ["sessions"] });
      await qc.invalidateQueries({ queryKey: ["api-tokens"] });
    },
    onError: (e) => setPwMsg({ tone: "danger", text: e instanceof Error ? e.message : "Could not change the password." }),
  });

  const sec = security.data;
  const others = sessions.data?.filter((s) => !s.current) ?? [];

  return (
    <div>
      <p className="text-[30px] leading-9 text-muted-foreground">Account</p>
      <h1 className="mt-6 border-b border-border pb-4 text-xl font-semibold">Security</h1>

      <div className="mt-8">
        <Section title="Two-factor authentication" description="Use an authenticator app to get a code when you sign in.">
          {!sec ? (
            <Skeleton className="h-16" />
          ) : (
            <div className="flex flex-col gap-3">
              <Row
                icon={<Phone />}
                title="Authenticator app"
                meta={sec.totpEnabled ? <Badge tone="success">On</Badge> : "Not set up"}
                action={
                  sec.totpEnabled ? (
                    <Button variant="danger-outline" onClick={() => setDisable(true)}>Disable 2FA</Button>
                  ) : (
                    <Button variant="outline" onClick={() => setEnroll(true)}>Set up</Button>
                  )
                }
              />
              {sec.totpEnabled ? (
                <Row
                  icon={<FileText />}
                  title="Recovery codes"
                  meta={`${sec.backupCodesRemaining} of 10 unused`}
                  action={<Button loading={regen.isPending} onClick={() => regen.mutate()}>Regenerate codes</Button>}
                />
              ) : null}
              <Row icon={<Key />} title="Passkeys" meta="Coming soon: sign in with your device." action={<Button disabled>Add passkey</Button>} />
            </div>
          )}
        </Section>

        <Section title="Password" description="Changing your password signs out every other session and, unless you untick it, revokes your API tokens.">
          <form
            className="flex max-w-md flex-col gap-3"
            onSubmit={(e) => {
              e.preventDefault();
              setPwMsg(null);
              changePw.mutate({ pw, keepTokens: !revokeTokens });
            }}
          >
            {pwMsg ? <Alert tone={pwMsg.tone}>{pwMsg.text}</Alert> : null}
            <Field label="New password" hint="At least 10 characters.">
              {(p) => <Input {...p} type="password" autoComplete="new-password" minLength={10} value={pw} onChange={(e) => setPw(e.target.value)} />}
            </Field>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={revokeTokens}
                onChange={(e) => setRevokeTokens(e.target.checked)}
                className="size-4 accent-[hsl(var(--accent))]"
              />
              Also revoke API tokens
            </label>
            <div>
              <Button type="submit" variant="primary" loading={changePw.isPending} disabled={pw.length < 10}>Update password</Button>
            </div>
          </form>
        </Section>

        <Section title="Sessions" description="Devices signed in to your account.">
          <div className="overflow-hidden rounded-lg border border-border">
            <table className="w-full text-sm">
              <thead className="bg-card text-left text-xs text-muted-foreground">
                <tr>
                  <th className="px-4 py-2 font-medium">Device</th>
                  <th className="px-4 py-2 font-medium">Last active</th>
                  <th className="px-4 py-2 font-medium">First used</th>
                  <th className="w-24" />
                </tr>
              </thead>
              <tbody>
                {sessions.data?.map((s) => (
                  <tr key={s.id} className="border-t border-border">
                    <td className="px-4 py-3">
                      <span className="flex items-center gap-2">
                        <World className="size-4 text-muted-foreground" />
                        {describeAgent(s.userAgent)}
                        {s.current ? <span className="text-muted-foreground">(this device)</span> : null}
                      </span>
                      <span className="ml-6 font-mono text-xs text-muted-foreground">{s.ip}</span>
                    </td>
                    <td className="px-4 py-3 text-muted-foreground">{relativeTime(s.lastSeenAt)}</td>
                    <td className="px-4 py-3 text-muted-foreground">{new Date(s.createdAt).toLocaleDateString()}</td>
                    <td className="px-4 py-3 text-right">
                      {!s.current ? (
                        <Button size="sm" variant="ghost" onClick={() => revoke.mutate(s.id)}>Revoke</Button>
                      ) : null}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {others.length > 0 ? (
            <Button
              className="mt-3"
              variant="danger-outline"
              onClick={async () => {
                for (const s of others) await api.revokeSession(s.id).catch(() => undefined);
                await qc.invalidateQueries({ queryKey: ["sessions"] });
              }}
            >
              Sign out all other sessions
            </Button>
          ) : null}
        </Section>

        <Section
          title="API tokens"
          description="For the modula CLI and automation. A token acts as you, optionally in one workspace only, and never with re-authentication."
        >
          <ApiTokens />
        </Section>
      </div>

      <EnrollDialog open={enroll} onOpenChange={setEnroll} />
      <Modal
        open={disable}
        onOpenChange={setDisable}
        title="Disable two-factor authentication?"
        description="Your account will be protected by your password only, and your recovery codes stop working."
        footer={
          <>
            <Button variant="ghost" onClick={() => setDisable(false)}>Cancel</Button>
            <Button variant="danger" loading={disableMut.isPending} onClick={() => disableMut.mutate()}>Disable 2FA</Button>
          </>
        }
      />
      <Modal open={!!newCodes} onOpenChange={(o) => !o && setNewCodes(null)} title="New recovery codes" description="Your old codes no longer work. Save these now; they won't be shown again." width={480}>
        {newCodes ? <RecoveryCodes codes={newCodes} /> : null}
      </Modal>
    </div>
  );
}
