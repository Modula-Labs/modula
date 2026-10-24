import { useNavigate } from "@tanstack/react-router";
import { useEffect } from "react";
import { Button } from "@/components/ui/Button";
import { Logo } from "@/components/ui/Logo";
import { Alert, Skeleton } from "@/components/ui/misc";
import { domainStepSkipped, skipDomainStep } from "@/lib/domain";
import { useMe } from "@/lib/workspace";
import { DomainForm, useInstanceDomain } from "./settings/Domain";

/** One-time "Add a domain" step after the first admin is created. Skippable; Settings → Domain stays available. */
export function DomainOnboardingPage() {
  const navigate = useNavigate();
  const me = useMe();
  const isAdmin = !!me.data?.user.isInstanceAdmin;
  const q = useInstanceDomain(isAdmin);
  const leave = () => void navigate({ to: "/projects" });
  const skipped = domainStepSkipped();
  const done = !!q.data?.dashboard;
  const skip = () => {
    skipDomainStep();
    leave();
  };

  useEffect(() => {
    if ((me.data && !isAdmin) || skipped || q.error) leave();
  }); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div className="dotgrid flex min-h-full flex-col items-center justify-center px-4 py-12">
      <div className="mb-8">
        <Logo />
      </div>
      <main className="anim-modal w-full max-w-[560px] rounded-lg border border-border bg-card p-8 shadow-modal">
        <h1 className="text-xl font-semibold">Add a domain</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          Give the dashboard a name and let services get addresses like api-acme-store.apps.example.com. Optional: you can keep using the server's IP address.
        </p>
        <div className="mt-6 flex flex-col gap-5">
          {q.isPending ? <Skeleton className="h-48" /> : q.data ? <DomainForm data={q.data} /> : null}
          {q.error ? <Alert>{q.error.message}</Alert> : null}
          <div className="flex items-center justify-between border-t border-border pt-4">
            <span className="text-[13px] text-muted-foreground">You can do this later in Settings → Domain.</span>
            {done ? (
              <Button variant="secondary" onClick={leave}>
                Continue
              </Button>
            ) : (
              <Button variant="ghost" onClick={skip}>
                Skip for now
              </Button>
            )}
          </div>
        </div>
      </main>
    </div>
  );
}
