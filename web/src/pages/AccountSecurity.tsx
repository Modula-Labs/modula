import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Copy,
  Download,
  FileText,
  Key,
  Phone,
  World,
} from "@/components/ui/icons";
import QRCode from "qrcode";
import { useEffect, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Card, CardContent, CardHeader } from "@/components/ui/Card";
import { Checkbox } from "@/components/ui/Controls";
import { Field, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { OtpInput } from "@/components/ui/OtpInput";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/Table";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type Passkey, type Security } from "@/lib/api";
import { describeAgent, relativeTime } from "@/lib/format";
import {
  createPasskey,
  passkeyError,
  passkeysSupported,
  rpMatchesHere,
} from "@/lib/webauthn";
import { ApiTokens } from "./AccountApiTokens";

function Section({
  title,
  description,
  children,
}: {
  title: string;
  description: string;
  children: ReactNode;
}) {
  return (
    <Card>
      <CardHeader
        title={<span className="font-bold">{title}</span>}
        description={description}
      />
      <CardContent>{children}</CardContent>
    </Card>
  );
}

function Row({
  icon,
  title,
  meta,
  action,
}: {
  icon: ReactNode;
  title: string;
  meta: ReactNode;
  action: ReactNode;
}) {
  return (
    <div className="flex items-center gap-3 rounded-lg px-3 py-2.5 hover:bg-muted/50">
      <span className="text-muted-foreground [&>svg]:size-5">{icon}</span>
      <div className="min-w-0 flex-1">
        <div className="text-sm">{title}</div>
        <div className="text-[13px] text-muted-foreground">{meta}</div>
      </div>
      {action}
    </div>
  );
}

export function RecoveryCodes({ codes }: { codes: string[] }) {
  const text = codes.join("\n");
  return (
    <div className="flex flex-col gap-3">
      <div className="grid grid-cols-2 gap-2 rounded-lg bg-muted p-4 font-mono text-[13px]">
        {codes.map((c) => (
          <span key={c}>{c}</span>
        ))}
      </div>
      <div className="flex gap-2">
        <Button
          size="sm"
          icon={<Copy />}
          onClick={() => void navigator.clipboard?.writeText(text)}
        >
          Copy
        </Button>
        <Button
          size="sm"
          icon={<Download />}
          onClick={() => {
            const url = URL.createObjectURL(
              new Blob([text + "\n"], { type: "text/plain" }),
            );
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

function EnrollDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
}) {
  const qc = useQueryClient();
  const [step, setStep] = useState<"scan" | "codes">("scan");
  const [enrollment, setEnrollment] = useState<{
    otpauthUrl: string;
    secret: string;
  } | null>(null);
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
        setQr(
          await QRCode.toDataURL(e.otpauthUrl, {
            margin: 1,
            width: 176,
            errorCorrectionLevel: "M",
          }),
        );
      })
      .catch((e) => {
        setError(
          e instanceof ApiError && e.code === "sudo_required"
            ? "Confirm it's you to continue."
            : (e as Error).message,
        );
      });
  }, [open]);

  const confirm = async (value = code) => {
    setBusy(true);
    setError(null);
    try {
      const r = await api.totpConfirm(value);
      await qc.invalidateQueries({ queryKey: ["security"] });
      // Codes come only with the first second factor; existing ones stay valid.
      if (r.backupCodes.length === 0) {
        onOpenChange(false);
        return;
      }
      setCodes(r.backupCodes);
      setStep("codes");
    } catch (e) {
      setError(
        e instanceof ApiError ? e.message : "Could not verify the code.",
      );
      setCode("");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onOpenChange={(o) =>
        step === "codes" && !saved ? undefined : onOpenChange(o)
      }
      title={
        step === "scan"
          ? "Set up authenticator app"
          : "Save your recovery codes"
      }
      description={
        step === "scan"
          ? "Scan the QR code with an authenticator app, then enter the 6-digit code it shows."
          : "Each code signs you in once if you lose your device. They won't be shown again."
      }
      width={480}
    >
      {error ? (
        <div className="mb-4">
          <Alert>{error}</Alert>
        </div>
      ) : null}
      {step === "scan" ? (
        <div className="flex flex-col gap-5">
          <div className="flex items-start gap-5">
            <div className="flex size-[176px] shrink-0 items-center justify-center rounded-md bg-white p-1">
              {qr ? (
                <img
                  src={qr}
                  alt="QR code for your authenticator app"
                  width={168}
                  height={168}
                />
              ) : (
                <Skeleton className="size-full" />
              )}
            </div>
            <div className="min-w-0 text-sm">
              <div className="text-muted-foreground">
                Can't scan? Enter this key manually:
              </div>
              <code className="mt-2 block rounded-lg bg-muted p-2 font-mono text-[13px] break-all select-all">
                {enrollment?.secret.match(/.{1,4}/g)?.join(" ") ?? "…"}
              </code>
            </div>
          </div>
          <div className="flex flex-col gap-2">
            <span className="text-sm">Code from the app</span>
            <OtpInput
              value={code}
              onChange={setCode}
              onComplete={(v) => void confirm(v)}
              disabled={busy || !enrollment}
              invalid={!!error}
            />
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button
              variant="primary"
              loading={busy}
              disabled={code.length !== 6}
              onClick={() => void confirm()}
            >
              Verify
            </Button>
          </div>
        </div>
      ) : (
        <div className="flex flex-col gap-5">
          <RecoveryCodes codes={codes} />
          <Checkbox
            checked={saved}
            onCheckedChange={setSaved}
            label="I saved these codes somewhere safe"
          />
          <div className="flex justify-end">
            <Button
              variant="primary"
              disabled={!saved}
              onClick={() => onOpenChange(false)}
            >
              Done
            </Button>
          </div>
        </div>
      )}
    </Modal>
  );
}

function AddPasskeyDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
}) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [codes, setCodes] = useState<string[] | null>(null);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setName(describeAgent(navigator.userAgent));
    setCodes(null);
    setSaved(false);
    setError(null);
  }, [open]);

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const options = await api.passkeyBegin();
      const credential = await createPasskey(options);
      const r = await api.passkeyConfirm(name.trim(), credential);
      await qc.invalidateQueries({ queryKey: ["security"] });
      if (r.backupCodes.length === 0) onOpenChange(false);
      else setCodes(r.backupCodes);
    } catch (e) {
      setError(e instanceof ApiError ? e.message : passkeyError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onOpenChange={(o) => (codes && !saved ? undefined : onOpenChange(o))}
      title={codes ? "Save your recovery codes" : "Add a passkey"}
      description={
        codes
          ? "If you lose your passkeys, each code signs you in once. They won't be shown again."
          : "Your device or security key asks you to confirm with a fingerprint, face or PIN. You'll use it as a second step when you sign in."
      }
      width={480}
    >
      {error ? (
        <div className="mb-4">
          <Alert>{error}</Alert>
        </div>
      ) : null}
      {codes ? (
        <div className="flex flex-col gap-5">
          <RecoveryCodes codes={codes} />
          <Checkbox
            checked={saved}
            onCheckedChange={setSaved}
            label="I saved these codes somewhere safe"
          />
          <div className="flex justify-end">
            <Button
              variant="primary"
              disabled={!saved}
              onClick={() => onOpenChange(false)}
            >
              Done
            </Button>
          </div>
        </div>
      ) : (
        <form
          className="flex flex-col gap-5"
          onSubmit={(e) => {
            e.preventDefault();
            void add();
          }}
        >
          <Field label="Name" hint="To tell your passkeys apart.">
            {(p) => (
              <Input
                {...p}
                autoFocus
                maxLength={100}
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
            )}
          </Field>
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              variant="ghost"
              onClick={() => onOpenChange(false)}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              variant="primary"
              icon={<Key />}
              loading={busy}
              disabled={!name.trim()}
            >
              Continue
            </Button>
          </div>
        </form>
      )}
    </Modal>
  );
}

function RenamePasskeyDialog({
  passkey,
  onClose,
}: {
  passkey: Passkey | null;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  useEffect(() => setName(passkey?.name ?? ""), [passkey]);
  const rename = useMutation({
    mutationFn: () => api.passkeyRename(passkey!.id, name.trim()),
    onSuccess: async () => {
      await qc.invalidateQueries({ queryKey: ["security"] });
      onClose();
    },
  });
  return (
    <Modal
      open={!!passkey}
      onOpenChange={(o) => !o && onClose()}
      title="Rename passkey"
    >
      <form
        className="flex flex-col gap-5"
        onSubmit={(e) => {
          e.preventDefault();
          rename.mutate();
        }}
      >
        {rename.error ? <Alert>{rename.error.message}</Alert> : null}
        <Field label="Name">
          {(p) => (
            <Input
              {...p}
              autoFocus
              maxLength={100}
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          )}
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            type="submit"
            variant="primary"
            loading={rename.isPending}
            disabled={!name.trim()}
          >
            Save
          </Button>
        </div>
      </form>
    </Modal>
  );
}

/** Why passkeys can't be added from here, if they can't. */
function passkeyBlocker(sec: Security): string | null {
  if (!passkeysSupported()) return "This browser doesn't support passkeys.";
  if (!sec.passkeyRpId)
    return "Passkeys need a dashboard domain. Set one in instance settings.";
  if (!rpMatchesHere(sec.passkeyRpId))
    return `Open the dashboard at https://${sec.passkeyRpId} to use passkeys.`;
  return null;
}

export function AccountSecurityPage() {
  const qc = useQueryClient();
  const security = useQuery({ queryKey: ["security"], queryFn: api.security });
  const sessions = useQuery({ queryKey: ["sessions"], queryFn: api.sessions });
  const [enroll, setEnroll] = useState(false);
  const [disable, setDisable] = useState(false);
  const [newCodes, setNewCodes] = useState<string[] | null>(null);
  const [addPasskey, setAddPasskey] = useState(false);
  const [renaming, setRenaming] = useState<Passkey | null>(null);
  const [removing, setRemoving] = useState<Passkey | null>(null);
  const [pw, setPw] = useState("");
  const [revokeTokens, setRevokeTokens] = useState(true);
  const [pwMsg, setPwMsg] = useState<{
    tone: "success" | "danger";
    text: string;
  } | null>(null);

  const disableMut = useMutation({
    mutationFn: api.totpDisable,
    onSuccess: async () => {
      setDisable(false);
      await qc.invalidateQueries({ queryKey: ["security"] });
    },
  });
  const removePasskey = useMutation({
    mutationFn: (id: string) => api.passkeyDelete(id),
    onSuccess: async () => {
      setRemoving(null);
      await qc.invalidateQueries({ queryKey: ["security"] });
    },
  });
  const regen = useMutation({
    mutationFn: api.backupCodes,
    onSuccess: (r) => setNewCodes(r.backupCodes),
  });
  const revoke = useMutation({
    mutationFn: api.revokeSession,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["sessions"] }),
  });
  const changePw = useMutation({
    mutationFn: (v: { pw: string; keepTokens: boolean }) =>
      api.changePassword(v.pw, v.keepTokens),
    onSuccess: async (r) => {
      setPw("");
      const tokens = r.tokensRevoked
        ? ` ${r.tokensRevoked} API token(s) revoked.`
        : "";
      setPwMsg({
        tone: "success",
        text: `Password changed. ${r.sessionsRevoked} other session(s) signed out.${tokens}`,
      });
      await qc.invalidateQueries({ queryKey: ["sessions"] });
      await qc.invalidateQueries({ queryKey: ["api-tokens"] });
    },
    onError: (e) =>
      setPwMsg({
        tone: "danger",
        text: e instanceof Error ? e.message : "Could not change the password.",
      }),
  });

  const sec = security.data;
  const twoFactor = !!sec && (sec.totpEnabled || sec.passkeys.length > 0);
  const blocker = sec ? passkeyBlocker(sec) : null;
  const others = sessions.data?.filter((s) => !s.current) ?? [];

  return (
    <div>
      <h1 className="text-2xl leading-8 font-bold">Security</h1>
      <p className="mt-1 text-sm text-muted-foreground">
        Sign-in methods, sessions and API tokens for your account.
      </p>

      <div className="mt-6 flex flex-col gap-6">
        <Section
          title="Two-factor authentication"
          description="After your password, confirm sign-ins with a passkey or an authenticator app. Recovery codes get you in if you lose both."
        >
          {!sec ? (
            <Skeleton className="h-16" />
          ) : (
            <div className="flex flex-col">
              <Row
                icon={<Phone />}
                title="Authenticator app"
                meta={
                  sec.totpEnabled ? (
                    <Badge tone="success">On</Badge>
                  ) : (
                    "Not set up"
                  )
                }
                action={
                  sec.totpEnabled ? (
                    <Button
                      variant="danger-outline"
                      onClick={() => setDisable(true)}
                    >
                      Turn off
                    </Button>
                  ) : (
                    <Button variant="outline" onClick={() => setEnroll(true)}>
                      Set up
                    </Button>
                  )
                }
              />
              <Row
                icon={<Key />}
                title="Passkeys"
                meta={
                  blocker ??
                  (sec.passkeys.length
                    ? `${sec.passkeys.length} added`
                    : "Use your device's fingerprint, face or PIN, or a security key.")
                }
                action={
                  <Button
                    variant="outline"
                    disabled={!!blocker}
                    onClick={() => setAddPasskey(true)}
                  >
                    Add passkey
                  </Button>
                }
              />
              {sec.passkeys.map((p) => (
                <div
                  key={p.id}
                  className="ml-11 flex items-center gap-3 rounded-lg px-3 py-2 hover:bg-muted/50"
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2 text-sm">
                      {p.name}
                      {p.rpId !== sec.passkeyRpId ? (
                        <Badge tone="warn">For {p.rpId}</Badge>
                      ) : null}
                    </div>
                    <div className="text-[13px] text-muted-foreground">
                      Added {new Date(p.createdAt).toLocaleDateString()}
                      {" · "}
                      {p.lastUsedAt
                        ? `last used ${relativeTime(p.lastUsedAt)}`
                        : "never used"}
                    </div>
                  </div>
                  <Button size="sm" variant="ghost" onClick={() => setRenaming(p)}>
                    Rename
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setRemoving(p)}>
                    Remove
                  </Button>
                </div>
              ))}
              {twoFactor ? (
                <Row
                  icon={<FileText />}
                  title="Recovery codes"
                  meta={`${sec.backupCodesRemaining} of 10 unused`}
                  action={
                    <Button
                      loading={regen.isPending}
                      onClick={() => regen.mutate()}
                    >
                      Regenerate codes
                    </Button>
                  }
                />
              ) : null}
            </div>
          )}
        </Section>

        <Section
          title="Password"
          description="Changing your password signs out every other session and, unless you untick it, revokes your API tokens."
        >
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
              {(p) => (
                <Input
                  {...p}
                  type="password"
                  autoComplete="new-password"
                  minLength={10}
                  value={pw}
                  onChange={(e) => setPw(e.target.value)}
                />
              )}
            </Field>
            <Checkbox
              checked={revokeTokens}
              onCheckedChange={setRevokeTokens}
              label="Also revoke API tokens"
            />
            <div>
              <Button
                type="submit"
                variant="primary"
                loading={changePw.isPending}
                disabled={pw.length < 10}
              >
                Update password
              </Button>
            </div>
          </form>
        </Section>

        <Section
          title="Sessions"
          description="Devices signed in to your account."
        >
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Device</TableHead>
                <TableHead>Last active</TableHead>
                <TableHead>First used</TableHead>
                <TableHead className="w-24" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {sessions.data?.map((s) => (
                <TableRow key={s.id}>
                  <TableCell className="h-auto py-2.5">
                    <span className="flex items-center gap-2">
                      <World className="size-4 text-muted-foreground" />
                      {describeAgent(s.userAgent)}
                      {s.current ? (
                        <span className="text-muted-foreground">
                          (this device)
                        </span>
                      ) : null}
                    </span>
                    <span className="ml-6 font-mono text-xs text-muted-foreground">
                      {s.ip}
                    </span>
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {relativeTime(s.lastSeenAt)}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {new Date(s.createdAt).toLocaleDateString()}
                  </TableCell>
                  <TableCell className="text-right">
                    {!s.current ? (
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => revoke.mutate(s.id)}
                      >
                        Revoke
                      </Button>
                    ) : null}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          {others.length > 0 ? (
            <Button
              className="mt-3"
              variant="danger-outline"
              onClick={async () => {
                for (const s of others)
                  await api.revokeSession(s.id).catch(() => undefined);
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
      <AddPasskeyDialog open={addPasskey} onOpenChange={setAddPasskey} />
      <RenamePasskeyDialog passkey={renaming} onClose={() => setRenaming(null)} />
      <Modal
        open={!!removing}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? "passkey"}?`}
        description={
          sec && !sec.totpEnabled && sec.passkeys.length === 1
            ? "This is your last second factor: your account will be protected by your password only, and your recovery codes stop working."
            : "It will no longer work for signing in."
        }
        footer={
          <>
            <Button variant="ghost" onClick={() => setRemoving(null)}>
              Cancel
            </Button>
            <Button
              variant="danger"
              loading={removePasskey.isPending}
              onClick={() => removing && removePasskey.mutate(removing.id)}
            >
              Remove
            </Button>
          </>
        }
      />
      <Modal
        open={disable}
        onOpenChange={setDisable}
        title="Turn off the authenticator app?"
        description={
          sec?.passkeys.length
            ? "Codes from your app stop working. Your passkeys and recovery codes still protect your account."
            : "Your account will be protected by your password only, and your recovery codes stop working."
        }
        footer={
          <>
            <Button variant="ghost" onClick={() => setDisable(false)}>
              Cancel
            </Button>
            <Button
              variant="danger"
              loading={disableMut.isPending}
              onClick={() => disableMut.mutate()}
            >
              Turn off
            </Button>
          </>
        }
      />
      <Modal
        open={!!newCodes}
        onOpenChange={(o) => !o && setNewCodes(null)}
        title="New recovery codes"
        description="Your old codes no longer work. Save these now; they won't be shown again."
        width={480}
      >
        {newCodes ? <RecoveryCodes codes={newCodes} /> : null}
      </Modal>
    </div>
  );
}
