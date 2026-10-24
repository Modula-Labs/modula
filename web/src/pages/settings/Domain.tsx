import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { World } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/Table";
import { Alert, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type InstanceDomain } from "@/lib/api";
import { dnsRecords, hostError, httpsUrl, normalizeHost } from "@/lib/domain";

export const instanceDomainKey = ["instance-domain"];

export function useInstanceDomain(enabled = true) {
  return useQuery({
    queryKey: instanceDomainKey,
    queryFn: api.instanceDomain,
    enabled,
    retry: (n, e) =>
      !(e instanceof ApiError && [403, 404].includes(e.status)) && n < 2,
  });
}

/** Settings → Domain (instance admins). */
export function DomainSection() {
  const q = useInstanceDomain();
  useEffect(() => {
    if (q.data && window.location.hash === "#domain")
      document.getElementById("domain")?.scrollIntoView();
  }, [q.data]);
  if (q.error instanceof ApiError && q.error.status === 404) return null;
  if (q.isPending) return <Skeleton className="h-40" />;
  if (q.error) return <Alert>{q.error.message}</Alert>;
  return <DomainForm data={q.data} />;
}

function AddressLink({ url, label }: { url: string; label: string }) {
  return (
    <div className="flex h-8 items-center gap-2 rounded-lg border border-border pr-1 pl-3">
      <World className="size-4 text-muted-foreground" />
      <a
        href={url}
        target="_blank"
        rel="noreferrer"
        className="min-w-0 flex-1 truncate font-mono text-[13px] hover:underline"
      >
        {url}
      </a>
      <span className="text-xs text-muted-foreground">{label}</span>
      <CopyButton value={url} label={`Copy ${url}`} />
    </div>
  );
}

/** Shared by Settings → Domain and the onboarding step. */
export function DomainForm({
  data,
  onSaved,
}: {
  data: InstanceDomain;
  onSaved?: (d: InstanceDomain) => void;
}) {
  const qc = useQueryClient();
  const [dashboard, setDashboard] = useState(data.dashboard ?? "");
  const [apps, setApps] = useState(data.appsDomain ?? "");
  const [justSaved, setJustSaved] = useState<string | null>(null);
  useEffect(() => {
    setDashboard(data.dashboard ?? "");
    setApps(data.appsDomain ?? "");
  }, [data.dashboard, data.appsDomain]);

  const dash = normalizeHost(dashboard);
  const app = normalizeHost(apps);
  const dashErr = hostError(dash);
  const appsErr = data.appsDomainFromEnv ? null : hostError(app);
  const dirty =
    dash !== (data.dashboard ?? "") ||
    (!data.appsDomainFromEnv && app !== (data.appsDomain ?? ""));
  const records = dnsRecords(
    dash,
    data.appsDomainFromEnv ? (data.appsDomain ?? "") : app,
    data.serverIp,
  );

  const put = useMutation({
    mutationFn: (v: {
      dashboard: string | null;
      appsDomain: string | null;
      skipDnsCheck?: boolean;
    }) => api.saveInstanceDomain(v),
    onSuccess: (d, v) => {
      qc.setQueryData(instanceDomainKey, d);
      setJustSaved(
        v.dashboard && v.dashboard !== data.dashboard ? v.dashboard : null,
      );
      onSaved?.(d);
    },
  });
  const appsValue = data.appsDomainFromEnv ? data.appsDomain : app || null;
  const submit = (skipDnsCheck?: boolean) =>
    put.mutate({
      dashboard: dash || null,
      appsDomain: appsValue,
      ...(skipDnsCheck ? { skipDnsCheck: true } : {}),
    });
  const conflict = put.error instanceof ApiError && put.error.status === 409;

  return (
    <form
      className="flex flex-col gap-5"
      onSubmit={(e) => {
        e.preventDefault();
        if (!dashErr && !appsErr) submit();
      }}
    >
      <div className="flex flex-col gap-2">
        <div className="text-sm font-bold">Dashboard address</div>
        {data.dashboard ? (
          <AddressLink url={httpsUrl(data.dashboard)} label="Domain" />
        ) : null}
        <AddressLink url={data.installUrl} label="Server IP" />
        <p className="text-[13px] text-muted-foreground">
          The IP address keeps working after you add a domain, so you can always
          get back in.
        </p>
      </div>

      <Field
        label="Dashboard domain"
        error={dashErr}
        hint="For example deploy.example.com."
      >
        {(p) => (
          <Input
            {...p}
            mono
            placeholder="deploy.example.com"
            spellCheck={false}
            autoComplete="off"
            value={dashboard}
            onChange={(e) => setDashboard(e.target.value)}
          />
        )}
      </Field>
      <Field
        label="Apps domain"
        error={appsErr}
        hint={
          data.appsDomainFromEnv
            ? "Set by the server configuration; change it there."
            : `Services get <name>.${app || "<apps domain>"}; needs a wildcard A record *.${app || "<apps domain>"}.`
        }
      >
        {(p) => (
          <Input
            {...p}
            mono
            placeholder="apps.example.com"
            spellCheck={false}
            autoComplete="off"
            readOnly={data.appsDomainFromEnv}
            value={data.appsDomainFromEnv ? (data.appsDomain ?? "") : apps}
            onChange={(e) => setApps(e.target.value)}
          />
        )}
      </Field>

      {records.length > 0 ? (
        <div className="flex flex-col gap-2">
          <div className="text-sm font-bold">DNS records to create</div>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Type</TableHead>
                <TableHead>Name</TableHead>
                <TableHead>Value</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {records.map((r) => (
                <TableRow key={r.name}>
                  <TableCell className="font-mono">{r.type}</TableCell>
                  <TableCell>
                    <span className="inline-flex max-w-full items-center gap-1">
                      <span className="truncate font-mono">{r.name}</span>
                      <CopyButton
                        value={r.name}
                        label={`Copy name ${r.name}`}
                      />
                    </span>
                  </TableCell>
                  <TableCell>
                    <span className="inline-flex max-w-full items-center gap-1">
                      <span className="truncate font-mono">{r.value}</span>
                      <CopyButton
                        value={r.value}
                        label={`Copy value ${r.value}`}
                      />
                    </span>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
          <p className="text-[13px] text-muted-foreground">
            Cloudflare: proxied (orange cloud) works. Set SSL/TLS to Full
            (strict); the server keeps its own Let's Encrypt certificate.
          </p>
        </div>
      ) : (dash || app) && !data.serverIp ? (
        <Alert tone="warn">
          This server's public IP is unknown, so the records can't be shown.
          Point the domain's A record at the server's IP address.
        </Alert>
      ) : null}

      {put.error ? (
        <Alert tone={conflict ? "warn" : "danger"}>{put.error.message}</Alert>
      ) : null}
      {justSaved && !put.error ? (
        <Alert tone="success" title="Saved">
          <a
            className="underline"
            href={httpsUrl(justSaved)}
            target="_blank"
            rel="noreferrer"
          >
            Open {httpsUrl(justSaved)}
          </a>{" "}
          once its certificate is ready. You may need to sign in again there.
        </Alert>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <Button
          type="submit"
          variant="primary"
          loading={put.isPending && !put.variables?.skipDnsCheck}
          disabled={!dirty || !!dashErr || !!appsErr}
        >
          Save
        </Button>
        {conflict ? (
          <Button
            type="button"
            variant="outline"
            loading={put.isPending && !!put.variables?.skipDnsCheck}
            disabled={!!dashErr || !!appsErr}
            onClick={() => submit(true)}
          >
            Save anyway
          </Button>
        ) : null}
        {data.dashboard ? (
          <a
            href={httpsUrl(data.dashboard)}
            target="_blank"
            rel="noreferrer"
            className="inline-flex h-8 items-center rounded-lg border border-border bg-background px-2.5 text-sm hover:bg-accent dark:border-input dark:bg-input/30 dark:hover:bg-input/50"
          >
            Open {httpsUrl(data.dashboard)}
          </a>
        ) : null}
        {data.dashboard ? (
          <Button
            type="button"
            variant="danger-outline"
            disabled={put.isPending}
            onClick={() =>
              put.mutate({ dashboard: null, appsDomain: appsValue })
            }
          >
            Remove dashboard domain
          </Button>
        ) : null}
        {data.appsDomain && !data.appsDomainFromEnv ? (
          <Button
            type="button"
            variant="danger-outline"
            disabled={put.isPending}
            onClick={() =>
              put.mutate({ dashboard: data.dashboard, appsDomain: null })
            }
          >
            Remove apps domain
          </Button>
        ) : null}
      </div>
      {conflict ? (
        <p className="-mt-3 text-[13px] text-muted-foreground">
          Save anyway skips the DNS check, for proxied Cloudflare records, which
          don't show the server's IP.
        </p>
      ) : null}
    </form>
  );
}
