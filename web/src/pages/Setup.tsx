import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { AuthCard } from "@/components/AuthCard";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { ApiError, api } from "@/lib/api";

function strength(pw: string): { score: 0 | 1 | 2 | 3; label: string } {
  if (pw.length < 10) return { score: 0, label: "At least 10 characters" };
  const classes = [/[a-z]/, /[A-Z]/, /\d/, /[^\w\s]/, /\s/].filter((r) => r.test(pw)).length;
  if (pw.length >= 16 || (pw.length >= 12 && classes >= 3)) return { score: 3, label: "Strong" };
  if (pw.length >= 12 || classes >= 3) return { score: 2, label: "Good" };
  return { score: 1, label: "Fair — longer is better" };
}

export function SetupPage() {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [form, setForm] = useState({ setupToken: "", name: "", email: "", password: "" });
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const s = strength(form.password);
  const set = (k: keyof typeof form) => (e: React.ChangeEvent<HTMLInputElement>) => setForm({ ...form, [k]: e.target.value });

  return (
    <AuthCard title="Set up Modula Deploy" subtitle="Create the first admin account. The setup token is printed in the server log.">
      <form
        className="flex flex-col gap-4"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(null);
          try {
            const me = await api.setup(form);
            qc.setQueryData(["me"], me);
            qc.setQueryData(["instance"], { setupRequired: false });
            await navigate({ to: "/projects" });
          } catch (err) {
            const ae = err instanceof ApiError ? err : null;
            setError(
              ae?.code === "invalid_credentials"
                ? "That setup token is invalid or expired. Restarting the server prints a new one."
                : ae?.code === "not_found"
                  ? "Setup is already complete."
                  : (ae?.message ?? "Something went wrong."),
            );
          } finally {
            setBusy(false);
          }
        }}
      >
        {error ? <Alert>{error}</Alert> : null}
        <Field label="Setup token">
          {(p) => <Input {...p} mono required autoFocus placeholder="mcsetup_…" autoComplete="off" spellCheck={false} value={form.setupToken} onChange={set("setupToken")} />}
        </Field>
        <Field label="Name">{(p) => <Input {...p} required autoComplete="name" value={form.name} onChange={set("name")} />}</Field>
        <Field label="Email">{(p) => <Input {...p} type="email" required autoComplete="email" value={form.email} onChange={set("email")} />}</Field>
        <Field label="Password" hint={s.label}>
          {(p) => <Input {...p} type="password" required minLength={10} autoComplete="new-password" value={form.password} onChange={set("password")} />}
        </Field>
        <div className="-mt-2 grid grid-cols-3 gap-1" aria-hidden>
          {[1, 2, 3].map((i) => (
            <div key={i} className={`h-1 rounded-full ${s.score >= i ? (s.score === 3 ? "bg-success" : s.score === 2 ? "bg-primary" : "bg-warning") : "bg-accent"}`} />
          ))}
        </div>
        <Button type="submit" variant="primary" size="lg" loading={busy} disabled={s.score === 0}>
          Create admin
        </Button>
      </form>
    </AuthCard>
  );
}
