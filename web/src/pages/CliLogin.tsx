import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { AuthCard } from "@/components/AuthCard";
import { Button } from "@/components/ui/Button";
import { Field, Input } from "@/components/ui/Input";
import { Alert, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type CliLoginRequest } from "@/lib/api";
import { cliCode } from "@/lib/cliLogin";
import { expiresIn } from "@/lib/members";

function errorText(e: unknown): string {
  const ae = e instanceof ApiError ? e : null;
  if (ae?.code === "rate_limited") return "Too many attempts. Wait a few minutes and try again.";
  return ae?.message ?? "Something went wrong.";
}

/** Where `modula login` sends the browser: approve (or deny) the terminal's
 *  login after comparing its code. Signed-out visitors sign in first and come back. */
export function CliLoginPage({ code }: { code?: string }) {
  const me = useQuery({ queryKey: ["me"], queryFn: api.me, retry: false });
  if (!code) return <EnterCode />;
  if (me.isLoading) {
    return (
      <AuthCard title="Log in to the CLI">
        <Skeleton className="h-24" />
      </AuthCard>
    );
  }
  if (!me.data) return <SignInFirst code={code} />;
  return <Review code={code} email={me.data.user.email} />;
}

function EnterCode() {
  const navigate = useNavigate();
  const [value, setValue] = useState("");
  const code = cliCode(value);
  return (
    <AuthCard title="Log in to the CLI" subtitle="Enter the code your terminal shows after `modula login`.">
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (code) void navigate({ to: "/cli/login", search: { code } });
        }}
      >
        <Field label="Code">
          {(p) => (
            <Input {...p} mono autoFocus autoComplete="off" spellCheck={false} placeholder="XXXX-XXXX" value={value} onChange={(e) => setValue(e.target.value)} />
          )}
        </Field>
        <Button type="submit" variant="primary" disabled={!code}>
          Continue
        </Button>
      </form>
    </AuthCard>
  );
}

function SignInFirst({ code }: { code: string }) {
  const navigate = useNavigate();
  return (
    <AuthCard title="Log in to the CLI" subtitle="Sign in to approve the login your terminal started.">
      <div className="flex flex-col gap-4">
        <CodeBox code={code} />
        <Button variant="primary" onClick={() => void navigate({ to: "/login", search: { cli: code } })}>
          Sign in
        </Button>
      </div>
    </AuthCard>
  );
}

function CodeBox({ code }: { code: string }) {
  return (
    <div className="rounded-lg bg-muted px-4 py-3 text-center font-mono text-2xl font-semibold tracking-[0.2em] text-foreground" aria-label={`Code ${code}`}>
      {code}
    </div>
  );
}

function Review({ code, email }: { code: string; email: string }) {
  const request = useQuery({
    queryKey: ["cli-login", code],
    queryFn: () => api.lookupCliLogin(code),
    retry: false,
    staleTime: Infinity,
  });
  const [done, setDone] = useState<"approved" | "denied" | null>(null);
  const [busy, setBusy] = useState<"approve" | "deny" | null>(null);
  const [error, setError] = useState<string | null>(null);

  if (done === "approved") {
    return (
      <AuthCard title="You're logged in" subtitle="Go back to your terminal: `modula` is ready to use. You can close this tab.">
        {null}
      </AuthCard>
    );
  }
  if (done === "denied") {
    return (
      <AuthCard title="Login denied" subtitle="The terminal didn't get access. If you didn't start this login, nothing else is needed.">
        {null}
      </AuthCard>
    );
  }
  if (request.isLoading) {
    return (
      <AuthCard title="Log in to the CLI">
        <Skeleton className="h-24" />
      </AuthCard>
    );
  }
  if (request.error || !request.data) {
    const notFound = request.error instanceof ApiError && request.error.status === 404;
    return (
      <AuthCard
        title="This login can't be approved"
        subtitle={notFound ? "The code is wrong, expired or already used. Run `modula login` again for a new one." : errorText(request.error)}
      >
        {null}
      </AuthCard>
    );
  }
  const r: CliLoginRequest = request.data;
  const act = async (what: "approve" | "deny") => {
    setBusy(what);
    setError(null);
    try {
      await (what === "approve" ? api.approveCliLogin(code) : api.denyCliLogin(code));
      setDone(what === "approve" ? "approved" : "denied");
    } catch (e) {
      setError(
        e instanceof ApiError && e.status === 404 ? "This login expired or was already used. Run `modula login` again." : errorText(e),
      );
    } finally {
      setBusy(null);
    }
  };
  return (
    <AuthCard
      title={`Approve CLI login from ${r.hostname}?`}
      subtitle={`Only approve if you just ran \`modula login\` and your terminal shows this code. It signs the CLI in as ${email}.`}
    >
      <div className="flex flex-col gap-4">
        {error ? <Alert>{error}</Alert> : null}
        <CodeBox code={r.userCode} />
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
          <dt className="text-muted-foreground">Machine</dt>
          <dd className="truncate">{r.hostname}</dd>
          <dt className="text-muted-foreground">From</dt>
          <dd className="font-mono text-[13px]">{r.ip}</dd>
          <dt className="text-muted-foreground">Code expires</dt>
          <dd>{expiresIn(r.expiresAt)}</dd>
        </dl>
        <p className="text-[13px] text-muted-foreground">
          This creates an API token named "CLI on {r.hostname}". Revoke it anytime under Account → Security.
        </p>
        <div className="flex justify-end gap-2 pt-2">
          <Button variant="outline" loading={busy === "deny"} disabled={!!busy} onClick={() => void act("deny")}>
            Deny
          </Button>
          <Button variant="primary" loading={busy === "approve"} disabled={!!busy} onClick={() => void act("approve")}>
            Approve
          </Button>
        </div>
      </div>
    </AuthCard>
  );
}
