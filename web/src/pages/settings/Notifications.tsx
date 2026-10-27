import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Checkbox, Switch } from "@/components/ui/Controls";
import { CopyButton } from "@/components/ui/CopyButton";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Plus } from "@/components/ui/icons";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/Table";
import { api } from "@/lib/api";
import {
  CHANNEL_KINDS,
  RULE_KINDS,
  alertsApi,
  channelKindLabel,
  channelStatus,
  describeRule,
  emptyRuleForm,
  isChannelUrl,
  kindSupportsServiceScope,
  phaseLabel,
  ruleFormBody,
  ruleKindInfo,
  ruleScope,
  sendStatus,
  testResultMessage,
  urlPlaceholder,
  validateRuleForm,
  type Channel,
  type ChannelKind,
  type Rule,
  type RuleForm,
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

/** Channels, rules and send history for the workspace's alerts. Members and up can use it. */
export function NotificationsSection({ workspaceId }: { workspaceId: string }) {
  return (
    <div className="flex flex-col gap-8">
      <ChannelsBlock workspaceId={workspaceId} />
      <RulesBlock workspaceId={workspaceId} />
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
      description="Where alerts are sent: a Slack or Discord incoming webhook, or any URL that accepts a signed POST."
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
      setResult(testResultMessage(r));
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
    },
  });
  const last = channelStatus(c);
  return (
    <li className="flex flex-col gap-2 rounded-lg border border-border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-bold">{c.name}</span>
        <Badge>{channelKindLabel(c.kind)}</Badge>
        <code className="min-w-0 flex-1 truncate font-mono text-[13px] text-muted-foreground">{c.host}</code>
        <Badge tone={last.tone}>{last.label}</Badge>
        {c.lastAt ? <span className="text-[13px] text-muted-foreground">{relativeTime(c.lastAt)}</span> : null}
      </div>
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

  const valid = name.trim() !== "" && isChannelUrl(url);
  return (
    <Modal open={open} onOpenChange={close} title="Add channel" description="Alerts are posted to this URL.">
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
            <Select {...p} value={kind} onChange={(e) => setKind(e.target.value as ChannelKind)}>
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
        <Field label="URL">
          {(p) => <Input {...p} mono type="url" placeholder={urlPlaceholder(kind)} value={url} onChange={(e) => setUrl(e.target.value)} />}
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
                set({ kind, serviceId: kindSupportsServiceScope(kind) ? f.serviceId : "" });
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
                  <TableCell title={s.title}>{s.ruleName || "Test message"}</TableCell>
                  <TableCell className="text-muted-foreground">{s.channelName}</TableCell>
                  <TableCell>{phaseLabel(s.phase)}</TableCell>
                  <TableCell>
                    <Badge tone={st.tone}>{st.label}</Badge>
                    {st.tone === "danger" && s.status !== null && s.error ? (
                      <span className="ml-2 text-[13px] text-muted-foreground">{s.error}</span>
                    ) : null}
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
