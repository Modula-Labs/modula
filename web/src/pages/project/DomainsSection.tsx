import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Plus, Refresh, Settings, Trash, World } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Alert, Badge } from "@/components/ui/misc";
import { api, type Domain } from "@/lib/api";
import { domainStatus, parsePort, parseRateLimit } from "./domains";
import { CloudflareIngress } from "../settings/CloudflareIngress";

/** Public domains of one service instance: generated ones and custom ones with DNS verification. */
export function DomainsSection({ instanceId, canEdit }: { instanceId: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const key = ["domains", instanceId];
  const domains = useQuery({
    queryKey: key,
    queryFn: () => api.domains(instanceId),
    // The server re-checks pending domains every minute; follow along.
    refetchInterval: (q) => (q.state.data?.some((d) => d.status !== "verified") ? 15_000 : false),
  });
  const invalidate = () => Promise.all([qc.invalidateQueries({ queryKey: key }), qc.invalidateQueries({ queryKey: ["project"] })]);
  const generate = useMutation({ mutationFn: () => api.generateDomain(instanceId), onSuccess: invalidate });
  const remove = useMutation({ mutationFn: (id: string) => api.deleteDomain(id), onSuccess: invalidate });
  const [adding, setAdding] = useState(false);
  const error = generate.error ?? remove.error;
  return (
    <div className="flex flex-col gap-2">
      <div className="text-sm font-medium">Public domains</div>
      {error ? <Alert>{error.message}</Alert> : null}
      {domains.data?.map((d) => (
        <DomainRow key={d.id} domain={d} canEdit={canEdit} onRemove={() => remove.mutate(d.id)} onChange={invalidate} />
      ))}
      {adding ? <AddCustomDomain instanceId={instanceId} onDone={() => setAdding(false)} onAdded={invalidate} /> : null}
      {canEdit && !adding ? (
        <div className="flex gap-2">
          <Button size="sm" variant="outline" icon={<Plus />} loading={generate.isPending} onClick={() => generate.mutate()}>
            Generate domain
          </Button>
          <Button size="sm" variant="outline" icon={<World />} onClick={() => setAdding(true)}>
            Custom domain
          </Button>
        </div>
      ) : null}
    </div>
  );
}

function DomainRow({ domain: d, canEdit, onRemove, onChange }: { domain: Domain; canEdit: boolean; onRemove: () => void; onChange: () => void }) {
  const status = domainStatus(d);
  const [settings, setSettings] = useState(false);
  const check = useMutation({ mutationFn: () => api.checkDomain(d.id), onSuccess: onChange });
  const live = d.status === "verified";
  return (
    <div className="rounded-md border border-border">
      <div className="flex h-10 items-center gap-2 pr-1 pl-3">
        <World className="size-4 text-muted-foreground" />
        {live ? (
          <a href={`${d.hostname.endsWith(".localhost") ? "http" : "https"}://${d.hostname}`} target="_blank" rel="noreferrer" className="min-w-0 flex-1 truncate text-sm hover:underline">
            {d.hostname}
          </a>
        ) : (
          <span className="min-w-0 flex-1 truncate text-sm">{d.hostname}</span>
        )}
        {d.targetPort ? <span className="font-mono text-xs text-muted-foreground">:{d.targetPort}</span> : null}
        {d.kind === "generated" ? <Badge>Generated</Badge> : null}
        {status ? <Badge tone={status.tone}>{status.label}</Badge> : null}
        <CopyButton value={d.hostname} label="Copy domain" />
        {canEdit && d.kind === "custom" ? (
          <button aria-label={`Settings for ${d.hostname}`} aria-expanded={settings} onClick={() => setSettings((s) => !s)} className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground">
            <Settings className="size-4" />
          </button>
        ) : null}
        {canEdit ? (
          <button aria-label={`Remove ${d.hostname}`} onClick={onRemove} className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-error">
            <Trash className="size-4" />
          </button>
        ) : null}
      </div>
      {d.kind === "custom" && !live ? (
        <div className="flex flex-col gap-2 border-t border-border px-3 py-3">
          <p className="text-[13px] text-muted-foreground">Add these records at your DNS provider. The domain goes live and gets a certificate once the TXT record is found.</p>
          <DnsRecords domain={d} />
          {d.lastError ? <p className={d.status === "failed" ? "text-[13px] text-error" : "text-[13px] text-muted-foreground"}>{d.lastError}</p> : null}
          {check.error ? <Alert>{check.error.message}</Alert> : null}
          {canEdit ? (
            <div>
              <Button size="sm" variant="outline" icon={<Refresh />} loading={check.isPending} onClick={() => check.mutate()}>
                Check now
              </Button>
            </div>
          ) : null}
        </div>
      ) : null}
      {d.kind === "custom" && live && d.dnsTargetOk === false ? (
        <p className="border-t border-border px-3 py-2 text-[13px] text-muted-foreground">
          The domain's {d.dnsRecords[0]?.type ?? "DNS"} record doesn't point here. That's expected behind a proxy such as Cloudflare; otherwise update it to the value below.
        </p>
      ) : null}
      {d.kind === "custom" ? <CloudflareIngress domainId={d.id} canEdit={canEdit} /> : null}
      {settings ? <EdgeSettings domain={d} onSaved={onChange} /> : null}
    </div>
  );
}

function DnsRecords({ domain: d }: { domain: Domain }) {
  if (d.dnsRecords.length === 0) return null;
  const routing = d.dnsRecords.some((r) => r.type !== "TXT");
  return (
    <div className="overflow-x-auto rounded-md border border-border">
      <table className="w-full text-[13px]">
        <thead className="text-caps text-muted-foreground">
          <tr className="border-b border-border">
            <th className="px-2.5 py-1.5 text-left font-normal">Type</th>
            <th className="px-2.5 py-1.5 text-left font-normal">Name</th>
            <th className="px-2.5 py-1.5 text-left font-normal">Value</th>
          </tr>
        </thead>
        <tbody>
          {d.dnsRecords.map((r) => (
            <tr key={`${r.type}-${r.name}-${r.value}`} className="border-b border-border last:border-0">
              <td className="px-2.5 py-1.5 font-mono">{r.type}</td>
              <td className="px-2.5 py-1.5">
                <span className="inline-flex max-w-full items-center gap-1">
                  <span className="truncate font-mono">{r.name}</span>
                  <CopyButton value={r.name} label={`Copy ${r.type} name`} />
                </span>
              </td>
              <td className="px-2.5 py-1.5">
                <span className="inline-flex max-w-full items-center gap-1">
                  <span className="truncate font-mono">{r.value}</span>
                  <CopyButton value={r.value} label={`Copy ${r.type} value`} />
                </span>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {routing ? null : <p className="px-2.5 py-1.5 text-[13px] text-muted-foreground">Deploy the service to see where to point the domain.</p>}
    </div>
  );
}

function AddCustomDomain({ instanceId, onDone, onAdded }: { instanceId: string; onDone: () => void; onAdded: () => void }) {
  const [hostname, setHostname] = useState("");
  const [port, setPort] = useState("");
  const parsed = parsePort(port);
  const add = useMutation({
    mutationFn: () => api.addCustomDomain(instanceId, { hostname, targetPort: "port" in parsed ? parsed.port : null }),
    onSuccess: () => {
      onAdded();
      onDone();
    },
  });
  return (
    <form
      className="flex flex-col gap-3 rounded-md border border-border p-3"
      onSubmit={(e) => {
        e.preventDefault();
        if ("port" in parsed && hostname.trim()) add.mutate();
      }}
    >
      <div className="flex flex-wrap items-start gap-3">
        <div className="min-w-48 flex-1">
          <Field label="Custom domain">{(p) => <Input {...p} mono autoFocus placeholder="app.example.com" value={hostname} onChange={(e) => setHostname(e.target.value)} />}</Field>
        </div>
        <div className="w-32">
          <Field label="Target port" hint="Blank: the service's port" error={"error" in parsed ? parsed.error : null}>
            {(p) => <Input {...p} inputMode="numeric" placeholder="auto" value={port} onChange={(e) => setPort(e.target.value)} />}
          </Field>
        </div>
      </div>
      {add.error ? <Alert>{add.error.message}</Alert> : null}
      <div className="flex gap-2">
        <Button size="sm" variant="secondary" type="submit" loading={add.isPending} disabled={!hostname.trim() || "error" in parsed}>
          Add domain
        </Button>
        <Button size="sm" variant="ghost" type="button" onClick={onDone}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

function EdgeSettings({ domain: d, onSaved }: { domain: Domain; onSaved: () => void }) {
  const [port, setPort] = useState(d.targetPort?.toString() ?? "");
  const [limit, setLimit] = useState(d.rateLimitPerMinute?.toString() ?? "");
  const p = parsePort(port);
  const l = parseRateLimit(limit);
  const save = useMutation({
    mutationFn: () => api.updateDomain(d.id, { targetPort: "port" in p ? p.port : null, rateLimitPerMinute: "limit" in l ? l.limit : null }),
    onSuccess: onSaved,
  });
  return (
    <div className="flex flex-col gap-3 border-t border-border px-3 py-3">
      <div className="flex flex-wrap items-start gap-3">
        <div className="w-32">
          <Field label="Target port" error={"error" in p ? p.error : null}>
            {(f) => <Input {...f} inputMode="numeric" placeholder="auto" value={port} onChange={(e) => setPort(e.target.value)} />}
          </Field>
        </div>
        <div className="w-48">
          <Field
            label="Rate limit (req/min per IP)"
            error={"error" in l ? l.error : null}
            hint={d.rateLimitEnforced ? undefined : "Saved, but not enforced yet: the edge doesn't include the rate-limit module."}
          >
            {(f) => <Input {...f} inputMode="numeric" placeholder="none" value={limit} onChange={(e) => setLimit(e.target.value)} />}
          </Field>
        </div>
      </div>
      {save.error ? <Alert>{save.error.message}</Alert> : null}
      <div>
        <Button size="sm" variant="secondary" loading={save.isPending} disabled={"error" in p || "error" in l} onClick={() => save.mutate()}>
          Save
        </Button>
      </div>
    </div>
  );
}
