import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Checkbox, Switch } from "@/components/ui/Controls";
import { CopyButton } from "@/components/ui/CopyButton";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { BrandDiscord, BrandSlack, Mail, Plus, Webhook } from "@/components/ui/icons";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/Table";
import { api } from "@/lib/api";
import {
  CHANNEL_KINDS,
  RULE_KINDS,
  alertsApi,
  browserTimezone,
  channelKindInfo,
  channelKindLabel,
  channelStatus,
  describeRule,
  emptyRuleForm,
  isChannelTarget,
  kindSupportsServiceScope,
  phaseLabel,
  reportBody,
  reportDeliveries,
  withKind,
  reportDirty,
  reportDraft,
  reportSchedule,
  ruleFormBody,
  ruleKindInfo,
  ruleScope,
  sendStatus,
  sendSubject,
  supportedTimezones,
  testResultMessage,
  timezoneOptions,
  validateReportDraft,
  validateRuleForm,
  type Channel,
  type ChannelKind,
  type ReportDelivery,
  type Rule,
  type RuleForm,
  type TrafficReport,
} from "@/lib/alerts";
import { relativeTime } from "@/lib/format";

function Block({ title, description, action, children }: { title: string; description: string; action?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-start justify-between gap-4">
        <div>
          <h3 className="text-base leading-6 font-bold">{title}</h3>
          <p className="text-sm text-muted-foreground">{description}</p>
        </div>
        {action}
      </div>
      {children}
    </div>
  );
}

const empty = (text: string) => (
  <p className="rounded-xl border border-dashed border-border px-4 py-6 text-center text-sm text-muted-foreground">{text}</p>
);

const kindIcons: Record<ChannelKind, ReactNode> = {
  slack: <BrandSlack />,
  discord: <BrandDiscord />,
  webhook: <Webhook />,
  email: <Mail />,
};

/** Channels, rules, the weekly report and send history for the workspace's alerts. Members and up can use it. */
export function NotificationsSection({ workspaceId }: { workspaceId: string }) {
  return (
    <div className="flex flex-col gap-8">
      <ChannelsBlock workspaceId={workspaceId} />
      <RulesBlock workspaceId={workspaceId} />
      <TrafficReportBlock workspaceId={workspaceId} />
      <HistoryBlock workspaceId={workspaceId} />
    </div>
  );
}

function ChannelsBlock({ workspaceId }: { workspaceId: string }) {
  const qc = useQueryClient();
  const key = ["alert-channels", workspaceId];
  const list = useQuery({ queryKey: key, queryFn: () => alertsApi.channels(workspaceId), retry: false });
  const [adding, setAdding] = useState(false);
  const changed = () => qc.invalidateQueries({ queryKey: key });
  return (
    <Block
      title="Channels"
      description="Where alerts are sent: a Slack or Discord incoming webhook, an email address, or any URL that accepts a signed POST."
      action={
        <Button icon={<Plus />} onClick={() => setAdding(true)}>
          Add channel
        </Button>
      }
    >
      {list.error ? <Alert tone="neutral">{list.error.message}</Alert> : null}
      {list.isLoading ? (
        <Skeleton className="h-16" />
      ) : list.data && list.data.length > 0 ? (
        <ul className="flex flex-col gap-2">
          {list.data.map((c) => (
            <ChannelRow key={c.id} channel={c} onChanged={() => void changed()} />
          ))}
        </ul>
      ) : list.data ? (
        empty("No channels yet.")
      ) : null}
      <AddChannelDialog
        workspaceId={workspaceId}
        open={adding}
        onOpenChange={setAdding}
        onCreated={() => void changed()}
      />
    </Block>
  );
}

function ChannelRow({ channel: c, onChanged }: { channel: Channel; onChanged: () => void }) {
  const qc = useQueryClient();
  const [confirming, setConfirming] = useState(false);
  const [result, setResult] = useState<ReturnType<typeof testResultMessage>>();
  const test = useMutation({
    mutationFn: () => alertsApi.testChannel(c.id),
    onSuccess: (r) => {
      setResult(testResultMessage(r, c.kind));
      onChanged();
      void qc.invalidateQueries({ queryKey: ["alert-history"] });
    },
    onError: () => setResult(undefined),
  });
  const remove = useMutation({
    mutationFn: () => alertsApi.deleteChannel(c.id),
    onSuccess: () => {
      onChanged();
      void qc.invalidateQueries({ queryKey: ["alert-rules"] });
      void qc.invalidateQueries({ queryKey: ["traffic-report"] });
    },
  });
  const last = channelStatus(c);
  return (
    <li className="flex flex-col gap-2 rounded-lg border border-border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-bold">{c.name}</span>
        <Badge icon={kindIcons[c.kind]}>{channelKindLabel(c.kind)}</Badge>
        <code className="min-w-0 flex-1 truncate font-mono text-[13px] text-muted-foreground">{c.host}</code>
        <Badge tone={last.tone}>{last.label}</Badge>
        {c.lastAt ? <span className="text-[13px] text-muted-foreground">{relativeTime(c.lastAt)}</span> : null}
      </div>
      {/* A failed test already says why, in the line below. */}
      {last.detail && !(result && !result.ok) ? <p className="text-[13px] text-muted-foreground">{last.detail}</p> : null}
      <div className="flex items-center justify-end gap-1.5">
        <Button variant="ghost" loading={test.isPending} onClick={() => test.mutate()}>
          Send test
        </Button>
        {confirming ? (
          <>
            <Button variant="ghost" onClick={() => setConfirming(false)}>
              Cancel
            </Button>
            <Button variant="danger" loading={remove.isPending} onClick={() => remove.mutate()}>
              Delete
            </Button>
          </>
        ) : (
          <Button variant="danger-outline" onClick={() => setConfirming(true)}>
            Delete
          </Button>
        )}
      </div>
      {test.error ? <Alert>{test.error.message}</Alert> : null}
      {remove.error ? <Alert>{remove.error.message}</Alert> : null}
      {result ? (
        <p role="status" className={result.ok ? "text-sm text-success" : "text-sm text-error"}>
          {result.text}
        </p>
      ) : null}
    </li>
  );
}

function AddChannelDialog({
  workspaceId,
  open,
  onOpenChange,
  onCreated,
}: {
  workspaceId: string;
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onCreated: () => void;
}) {
  const [kind, setKind] = useState<ChannelKind>("slack");
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  // The signing secret is returned once, by the create call.
  const [secret, setSecret] = useState<string>();
  const create = useMutation({
    mutationFn: () => alertsApi.createChannel(workspaceId, { kind, name: name.trim(), url: url.trim() }),
    onSuccess: (c) => {
      onCreated();
      if (c.secret) setSecret(c.secret);
      else close(false);
    },
  });
  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) {
      setKind("slack");
      setName("");
      setUrl("");
      setSecret(undefined);
      create.reset();
    }
  };

  if (secret) {
    return (
      <Modal
        open={open}
        onOpenChange={close}
        title="Copy the signing secret now"
        description="It is shown only once."
        footer={<Button variant="primary" onClick={() => close(false)}>Done</Button>}
      >
        <div className="flex flex-col gap-2 text-sm">
          <p>
            Every delivery carries an <code className="font-mono">X-Modula-Signature: sha256=&lt;hex&gt;</code> header: the HMAC-SHA256 of the request body, keyed with
            this secret. Verify it before trusting the payload.
          </p>
          <div className="flex items-center gap-1 rounded-lg border border-input bg-background py-0.5 pr-0.5 pl-2.5 dark:bg-input/30">
            <code className="min-w-0 flex-1 font-mono text-[13px] break-all">{secret}</code>
            <CopyButton value={secret} label="Copy signing secret" />
          </div>
        </div>
      </Modal>
    );
  }

  const info = channelKindInfo(kind) ?? CHANNEL_KINDS[0]!;
  const valid = name.trim() !== "" && isChannelTarget(kind, url);
  return (
    <Modal open={open} onOpenChange={close} title="Add channel" description={info.describe}>
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) create.mutate();
        }}
      >
        {create.error ? <Alert>{create.error.message}</Alert> : null}
        <Field label="Type">
          {(p) => (
            <Select
              {...p}
              value={kind}
              onChange={(e) => {
                // A URL is not an address: start the field over for the new kind.
                setKind(e.target.value as ChannelKind);
                setUrl("");
              }}
            >
              {CHANNEL_KINDS.map((k) => (
                <option key={k.value} value={k.value}>
                  {k.label}
                </option>
              ))}
            </Select>
          )}
        </Field>
        <Field label="Name">
          {(p) => <Input {...p} autoFocus placeholder="#ops" value={name} onChange={(e) => setName(e.target.value)} />}
        </Field>
        <Field label={info.inputLabel} hint={info.hint}>
          {(p) => (
            <Input
              {...p}
              mono={info.input === "url"}
              type={info.input}
              autoComplete={info.input === "email" ? "email" : undefined}
              placeholder={info.placeholder}
              value={url}
              onChange={(e) => setUrl(e.target.value)}
            />
          )}
        </Field>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => close(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending} disabled={!valid}>
            Add channel
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function RulesBlock({ workspaceId }: { workspaceId: string }) {
  const qc = useQueryClient();
  const key = ["alert-rules", workspaceId];
  const list = useQuery({ queryKey: key, queryFn: () => alertsApi.rules(workspaceId), retry: false });
  const channels = useQuery({ queryKey: ["alert-channels", workspaceId], queryFn: () => alertsApi.channels(workspaceId), retry: false });
  const [adding, setAdding] = useState(false);
  const changed = () => qc.invalidateQueries({ queryKey: key });
  const channelName = (id: string) => channels.data?.find((c) => c.id === id)?.name ?? id;
  return (
    <Block
      title="Rules"
      description="When to alert, for what, and which channels get it. A rule that is still firing doesn't repeat until its cooldown has passed."
      action={
        <Button icon={<Plus />} disabled={!channels.data || channels.data.length === 0} onClick={() => setAdding(true)}>
          Add rule
        </Button>
      }
    >
      {list.error ? <Alert tone="neutral">{list.error.message}</Alert> : null}
      {list.isLoading ? (
        <Skeleton className="h-16" />
      ) : list.data && list.data.length > 0 ? (
        <ul className="flex flex-col gap-2">
          {list.data.map((r) => (
            <RuleRow key={r.id} rule={r} channelName={channelName} onChanged={() => void changed()} />
          ))}
        </ul>
      ) : list.data ? (
        empty(channels.data && channels.data.length === 0 ? "Add a channel first, then create rules." : "No rules yet.")
      ) : null}
      <AddRuleDialog
        workspaceId={workspaceId}
        channels={channels.data ?? []}
        open={adding}
        onOpenChange={setAdding}
        onCreated={() => void changed()}
      />
    </Block>
  );
}

/** One rule: what it watches, where, who is told, and a switch. */
function RuleRow({ rule: r, channelName, onChanged }: { rule: Rule; channelName: (id: string) => string; onChanged: () => void }) {
  const [confirming, setConfirming] = useState(false);
  const toggle = useMutation({ mutationFn: (enabled: boolean) => alertsApi.updateRule(r.id, { enabled }), onSuccess: onChanged });
  const remove = useMutation({ mutationFn: () => alertsApi.deleteRule(r.id), onSuccess: onChanged });
  return (
    <li className="flex flex-col gap-2 rounded-lg border border-border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-bold">{r.name}</span>
        {r.firing > 0 ? <Badge tone="danger">{r.firing} firing</Badge> : null}
        <span className="flex-1" />
        <Switch
          checked={r.enabled}
          disabled={toggle.isPending}
          onCheckedChange={(v) => toggle.mutate(v)}
          label={r.enabled ? "Enabled" : "Disabled"}
        />
      </div>
      <div className="text-sm">{describeRule(r)}</div>
      <div className="flex flex-wrap items-center gap-1.5 text-[13px] text-muted-foreground">
        <span>{ruleScope(r)}</span>
        <span aria-hidden>·</span>
        <span>Cooldown {r.cooldownMinutes} min</span>
        <span className="flex-1" />
        {r.channelIds.map((id) => (
          <Badge key={id}>{channelName(id)}</Badge>
        ))}
        {confirming ? (
          <>
            <Button variant="ghost" onClick={() => setConfirming(false)}>
              Cancel
            </Button>
            <Button variant="danger" loading={remove.isPending} onClick={() => remove.mutate()}>
              Delete
            </Button>
          </>
        ) : (
          <Button variant="danger-outline" onClick={() => setConfirming(true)}>
            Delete
          </Button>
        )}
      </div>
      {toggle.error ? <Alert>{toggle.error.message}</Alert> : null}
      {remove.error ? <Alert>{remove.error.message}</Alert> : null}
    </li>
  );
}

function AddRuleDialog({
  workspaceId,
  channels,
  open,
  onOpenChange,
  onCreated,
}: {
  workspaceId: string;
  channels: Channel[];
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onCreated: () => void;
}) {
  const [f, setF] = useState<RuleForm>(emptyRuleForm);
  const [touched, setTouched] = useState(false);
  const set = (p: Partial<RuleForm>) => setF((cur) => ({ ...cur, ...p }));
  const projects = useQuery({ queryKey: ["projects", workspaceId], queryFn: () => api.projects(workspaceId), enabled: open, retry: false });
  const project = projects.data?.find((p) => p.id === f.projectId);
  const info = ruleKindInfo(f.kind);
  const close = (o: boolean) => {
    onOpenChange(o);
    if (!o) {
      setF(emptyRuleForm());
      setTouched(false);
      create.reset();
    }
  };
  const create = useMutation({
    mutationFn: () => alertsApi.createRule(workspaceId, ruleFormBody(f)),
    onSuccess: () => {
      onCreated();
      close(false);
    },
  });
  const problem = validateRuleForm(f);
  const toggleChannel = (id: string) =>
    set({ channelIds: f.channelIds.includes(id) ? f.channelIds.filter((x) => x !== id) : [...f.channelIds, id] });

  return (
    <Modal open={open} onOpenChange={close} title="Add rule" description="Choose what to watch and who to tell." width={512}>
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          setTouched(true);
          if (!problem) create.mutate();
        }}
      >
        {create.error ? <Alert>{create.error.message}</Alert> : null}
        {touched && problem ? <Alert tone="warn">{problem}</Alert> : null}
        <Field label="Name">
          {(p) => <Input {...p} autoFocus placeholder="Production CPU" value={f.name} onChange={(e) => set({ name: e.target.value })} />}
        </Field>
        <Field label="Alert when" hint={info.hint}>
          {(p) => (
            <Select
              {...p}
              value={f.kind}
              onChange={(e) => {
                const kind = e.target.value as typeof f.kind;
                set({ ...withKind(f, kind), serviceId: kindSupportsServiceScope(kind) ? f.serviceId : "" });
              }}
            >
              {RULE_KINDS.map((k) => (
                <option key={k.value} value={k.value}>
                  {k.label}
                </option>
              ))}
            </Select>
          )}
        </Field>
        <Field label="Scope" hint={projects.error ? "Projects couldn't be loaded." : undefined}>
          {(p) => (
            <Select {...p} value={f.projectId} onChange={(e) => set({ projectId: e.target.value, serviceId: "" })}>
              <option value="">Whole workspace</option>
              {projects.data?.map((pr) => (
                <option key={pr.id} value={pr.id}>
                  {pr.name}
                </option>
              ))}
            </Select>
          )}
        </Field>
        {project && kindSupportsServiceScope(f.kind) ? (
          <Field label="Service">
            {(p) => (
              <Select {...p} value={f.serviceId} onChange={(e) => set({ serviceId: e.target.value })}>
                <option value="">Every service in {project.name}</option>
                {project.services.map((s) => (
                  <option key={s.id} value={s.id}>
                    {s.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        ) : null}
        {info.threshold || info.window ? (
          <div className="grid grid-cols-2 gap-4">
            {info.threshold ? (
              <Field label="Threshold (%)">
                {(p) => <Input {...p} inputMode="numeric" value={f.threshold} onChange={(e) => set({ threshold: e.target.value })} />}
              </Field>
            ) : null}
            {info.window ? (
              <Field label={f.kind === "node_offline" ? "Offline for (minutes)" : "For (minutes)"} hint={`${info.window.min} to ${info.window.max}`}>
                {(p) => <Input {...p} inputMode="numeric" value={f.windowMinutes} onChange={(e) => set({ windowMinutes: e.target.value })} />}
              </Field>
            ) : null}
          </div>
        ) : null}
        <Field label="Cooldown (minutes)" hint="Minimum time between repeat alerts for the same thing.">
          {(p) => <Input {...p} className="max-w-32" inputMode="numeric" value={f.cooldownMinutes} onChange={(e) => set({ cooldownMinutes: e.target.value })} />}
        </Field>
        <fieldset className="flex flex-col gap-2">
          <legend className="mb-1 text-sm">Channels</legend>
          {channels.map((c) => (
            <Checkbox
              key={c.id}
              checked={f.channelIds.includes(c.id)}
              onCheckedChange={() => toggleChannel(c.id)}
              label={
                <>
                  {c.name} <span className="text-muted-foreground">{channelKindLabel(c.kind)}</span>
                </>
              }
            />
          ))}
        </fieldset>
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => close(false)}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" loading={create.isPending}>
            Add rule
          </Button>
        </div>
      </form>
    </Modal>
  );
}

function TrafficReportBlock({ workspaceId }: { workspaceId: string }) {
  const report = useQuery({ queryKey: ["traffic-report", workspaceId], queryFn: () => alertsApi.trafficReport(workspaceId), retry: false });
  const channels = useQuery({ queryKey: ["alert-channels", workspaceId], queryFn: () => alertsApi.channels(workspaceId), retry: false });
  return (
    <Block
      title="Weekly traffic report"
      description="Requests, errors and latency for the last 7 days against the week before, and the busiest services. It goes to the channels you pick."
    >
      {report.error ? <Alert tone="neutral">{report.error.message}</Alert> : null}
      {channels.error ? <Alert tone="neutral">{channels.error.message}</Alert> : null}
      {report.isLoading || channels.isLoading ? (
        <Skeleton className="h-24" />
      ) : report.data && channels.data ? (
        <TrafficReportForm key={workspaceId} workspaceId={workspaceId} report={report.data} channels={channels.data} />
      ) : null}
    </Block>
  );
}

/** The report's switch, timezone and channels as one form, saved together; "Send now" sends what is saved. */
function TrafficReportForm({ workspaceId, report, channels }: { workspaceId: string; report: TrafficReport; channels: Channel[] }) {
  const qc = useQueryClient();
  const key = ["traffic-report", workspaceId];
  const browserTz = useMemo(browserTimezone, []);
  const supported = useMemo(supportedTimezones, []);
  // The form starts from what loaded; later refetches don't overwrite what is being typed.
  const [draft, setDraft] = useState(() => reportDraft(report, browserTz));
  const [touched, setTouched] = useState(false);
  const [sent, setSent] = useState<ReportDelivery[]>();
  const known = channels.map((c) => c.id);
  // A channel deleted meanwhile is no longer part of the form.
  const cur = { ...draft, channelIds: draft.channelIds.filter((id) => known.includes(id)) };
  const dirty = reportDirty(cur, report, browserTz);
  const problem = validateReportDraft(cur);
  const zones = useMemo(() => timezoneOptions(supported, cur.timezone), [supported, cur.timezone]);
  const savedChannelIds = report.channelIds.filter((id) => known.includes(id));
  // The saved next run only describes the saved timezone.
  const nextAt = report.enabled && cur.enabled && cur.timezone === report.timezone ? report.nextAt : null;

  const save = useMutation({
    mutationFn: () => alertsApi.setTrafficReport(workspaceId, reportBody(cur, known)),
    onSuccess: (saved) => {
      qc.setQueryData(key, saved);
      setDraft(reportDraft(saved, browserTz));
      setTouched(false);
      setSent(undefined);
    },
  });
  const send = useMutation({
    mutationFn: () => alertsApi.sendTrafficReport(workspaceId),
    onMutate: () => setSent(undefined),
    onSuccess: (results) => {
      setSent(reportDeliveries(channels, report.channelIds, results));
      void qc.invalidateQueries({ queryKey: ["alert-history"] });
      void qc.invalidateQueries({ queryKey: ["alert-channels", workspaceId] });
    },
  });
  const toggleChannel = (id: string) =>
    setDraft({ ...cur, channelIds: cur.channelIds.includes(id) ? cur.channelIds.filter((x) => x !== id) : [...cur.channelIds, id] });

  return (
    <form
      aria-label="Weekly traffic report"
      className="flex flex-col gap-4 rounded-lg border border-border p-3"
      onSubmit={(e) => {
        e.preventDefault();
        setTouched(true);
        if (!problem && dirty) save.mutate();
      }}
    >
      {save.error ? <Alert>{save.error.message}</Alert> : null}
      {touched && problem ? <Alert tone="warn">{problem}</Alert> : null}
      <Switch
        checked={cur.enabled}
        disabled={channels.length === 0 && !cur.enabled}
        onCheckedChange={(enabled) => setDraft({ ...cur, enabled })}
        label={cur.enabled ? "Enabled" : "Disabled"}
      />
      <Field label="Timezone" hint={cur.timezone === browserTz ? "Your browser's timezone." : undefined}>
        {(p) => (
          <Select {...p} className="max-w-xs" value={cur.timezone} onChange={(e) => setDraft({ ...cur, timezone: e.target.value })}>
            {zones.map((z) => (
              <option key={z} value={z}>
                {z}
              </option>
            ))}
          </Select>
        )}
      </Field>
      <fieldset className="flex flex-col gap-2">
        <legend className="mb-1 text-sm">Channels</legend>
        {channels.length === 0 ? (
          <p className="text-sm text-muted-foreground">Add a channel first, then pick it here.</p>
        ) : (
          channels.map((c) => (
            <Checkbox
              key={c.id}
              checked={cur.channelIds.includes(c.id)}
              onCheckedChange={() => toggleChannel(c.id)}
              label={
                <>
                  {c.name} <span className="text-muted-foreground">{channelKindLabel(c.kind)}</span>
                </>
              }
            />
          ))
        )}
      </fieldset>
      <p className="text-sm text-muted-foreground">
        {reportSchedule(cur.timezone, nextAt)}
        {report.lastSentAt ? ` · last sent ${relativeTime(report.lastSentAt)}` : ""}
      </p>
      {send.error ? <Alert>{send.error.message}</Alert> : null}
      {sent ? (
        <ul role="status" className="flex flex-col gap-1">
          {sent.map((d, i) => (
            <li key={i} className={d.ok ? "text-sm text-success" : "text-sm text-error"}>
              {d.text}
            </li>
          ))}
        </ul>
      ) : null}
      <div className="flex flex-wrap items-center justify-end gap-2">
        {dirty ? <span className="mr-auto text-sm text-muted-foreground">Save your changes to send with them.</span> : null}
        <Button
          type="button"
          loading={send.isPending}
          disabled={dirty || savedChannelIds.length === 0}
          onClick={() => send.mutate()}
        >
          Send now
        </Button>
        <Button type="submit" variant="primary" loading={save.isPending} disabled={!dirty}>
          Save
        </Button>
      </div>
    </form>
  );
}

function HistoryBlock({ workspaceId }: { workspaceId: string }) {
  const history = useQuery({ queryKey: ["alert-history", workspaceId], queryFn: () => alertsApi.history(workspaceId), retry: false });
  return (
    <Block title="History" description="The last 100 messages sent to your channels.">
      {history.error ? <Alert tone="neutral">{history.error.message}</Alert> : null}
      {history.isLoading ? (
        <Skeleton className="h-16" />
      ) : history.data && history.data.length > 0 ? (
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Time</TableHead>
              <TableHead>Rule</TableHead>
              <TableHead>Channel</TableHead>
              <TableHead>Phase</TableHead>
              <TableHead>Status</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {history.data.map((s) => {
              const st = sendStatus(s);
              return (
                <TableRow key={s.id}>
                  <TableCell className="whitespace-nowrap text-muted-foreground" title={s.at}>
                    {relativeTime(s.at)}
                  </TableCell>
                  <TableCell title={s.title}>{sendSubject(s)}</TableCell>
                  <TableCell className="text-muted-foreground">{s.channelName}</TableCell>
                  <TableCell>{phaseLabel(s.phase)}</TableCell>
                  <TableCell>
                    <Badge tone={st.tone}>{st.label}</Badge>
                    {st.detail ? <div className="mt-1 max-w-md text-[13px] whitespace-normal text-muted-foreground">{st.detail}</div> : null}
                  </TableCell>
                </TableRow>
              );
            })}
          </TableBody>
        </Table>
      ) : history.data ? (
        empty("Nothing has been sent yet.")
      ) : null}
    </Block>
  );
}
