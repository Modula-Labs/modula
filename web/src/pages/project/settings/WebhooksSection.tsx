import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Plus } from "@/components/ui/icons";
import { Checkbox } from "@/components/ui/Controls";
import { Field, Input } from "@/components/ui/Input";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { api, type Webhook, type WebhookEvent } from "@/lib/api";
import { relativeTime } from "@/lib/format";
import { WEBHOOK_EVENTS, deliveryStatus, isWebhookUrl } from "@/lib/webhooks";
import { SettingsSection } from "./SettingsParts";

const ALL_EVENTS = WEBHOOK_EVENTS.map((e) => e.value);

export function WebhooksSection({ projectId, canEdit }: { projectId: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const key = ["webhooks", projectId];
  const list = useQuery({ queryKey: key, queryFn: () => api.webhooks(projectId) });
  const [url, setUrl] = useState("");
  const [events, setEvents] = useState<WebhookEvent[]>(ALL_EVENTS);
  // The signing secret is returned once, by the create call.
  const [created, setCreated] = useState<{ url: string; secret: string }>();
  const create = useMutation({
    mutationFn: () => api.createWebhook(projectId, { url: url.trim(), events }),
    onSuccess: async (w) => {
      setCreated({ url: w.url, secret: w.secret });
      setUrl("");
      setEvents(ALL_EVENTS);
      await qc.invalidateQueries({ queryKey: key });
    },
  });
  const toggle = (e: WebhookEvent) => setEvents((cur) => (cur.includes(e) ? cur.filter((x) => x !== e) : [...cur, e]));

  return (
    <SettingsSection title="Webhooks" description="Get a POST request to your URL when a deployment in this project succeeds, fails or crashes.">
      <div className="flex max-w-2xl flex-col gap-4">
        {created ? (
          <Alert tone="info" title="Copy the signing secret now">
            <div className="flex flex-col gap-2">
              <p>
                It is shown only once. Every delivery to {created.url} carries an <code className="font-mono">X-Modula-Signature</code> header with the HMAC-SHA256 of
                the request body, keyed with this secret. Verify it before trusting the payload.
              </p>
              <div className="flex items-center gap-1 rounded-lg border border-input bg-background py-0.5 pr-0.5 pl-2.5 dark:bg-input/30">
                <code className="min-w-0 flex-1 font-mono text-[13px] break-all">{created.secret}</code>
                <CopyButton value={created.secret} label="Copy signing secret" />
              </div>
              <div>
                <Button variant="ghost" onClick={() => setCreated(undefined)}>
                  Done
                </Button>
              </div>
            </div>
          </Alert>
        ) : null}

        {list.error ? <Alert>{list.error.message}</Alert> : null}
        {list.isLoading ? (
          <Skeleton className="h-16" />
        ) : list.data && list.data.length > 0 ? (
          <ul className="flex flex-col gap-2">
            {list.data.map((w) => (
              <WebhookRow key={w.id} webhook={w} canEdit={canEdit} onChanged={() => qc.invalidateQueries({ queryKey: key })} />
            ))}
          </ul>
        ) : list.data ? (
          <p className="rounded-xl border border-dashed border-border px-4 py-6 text-center text-sm text-muted-foreground">No webhooks yet.</p>
        ) : null}

        {canEdit ? (
          <form
            className="flex flex-col gap-4 rounded-xl border border-border p-4"
            onSubmit={(e) => {
              e.preventDefault();
              create.mutate();
            }}
          >
            <div className="text-base leading-6 font-bold">Add a webhook</div>
            {create.error ? <Alert>{create.error.message}</Alert> : null}
            <Field label="Endpoint URL">
              {(p) => <Input {...p} mono type="url" required placeholder="https://example.com/hooks/modula" value={url} onChange={(e) => setUrl(e.target.value)} />}
            </Field>
            <fieldset className="flex flex-col gap-2">
              <legend className="mb-1 text-sm font-bold">Events</legend>
              {WEBHOOK_EVENTS.map((e) => (
                <Checkbox
                  key={e.value}
                  checked={events.includes(e.value)}
                  onCheckedChange={() => toggle(e.value)}
                  label={
                    <>
                      {e.label} <span className="text-muted-foreground">{e.hint}</span>
                    </>
                  }
                />
              ))}
            </fieldset>
            <div>
              <Button type="submit" variant="primary" icon={<Plus />} loading={create.isPending} disabled={!isWebhookUrl(url) || events.length === 0}>
                Add webhook
              </Button>
            </div>
          </form>
        ) : null}
      </div>
    </SettingsSection>
  );
}

function WebhookRow({ webhook: w, canEdit, onChanged }: { webhook: Webhook; canEdit: boolean; onChanged: () => void }) {
  const [confirming, setConfirming] = useState(false);
  const [result, setResult] = useState<{ status: number | null; error: string | null }>();
  const test = useMutation({
    mutationFn: () => api.testWebhook(w.id),
    onSuccess: (r) => {
      setResult(r);
      onChanged();
    },
  });
  const remove = useMutation({ mutationFn: () => api.deleteWebhook(w.id), onSuccess: onChanged });
  const last = deliveryStatus(w);
  const ok = result && result.status !== null && result.status >= 200 && result.status < 300;
  return (
    <li className="flex flex-col gap-2 rounded-lg border border-border p-3">
      <div className="flex items-center gap-2">
        <code className="min-w-0 flex-1 truncate font-mono text-[13px]" title={w.url}>
          {w.url}
        </code>
        <Badge tone={last.tone}>{last.label}</Badge>
        {w.lastAt ? <span className="text-[13px] text-muted-foreground">{relativeTime(w.lastAt)}</span> : null}
      </div>
      <div className="flex flex-wrap items-center gap-1.5">
        {w.events.map((e) => (
          <Badge key={e}>{e}</Badge>
        ))}
        <span className="flex-1" />
        {canEdit ? (
          <>
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
          </>
        ) : null}
      </div>
      {test.error ? <Alert>{test.error.message}</Alert> : null}
      {remove.error ? <Alert>{remove.error.message}</Alert> : null}
      {result ? (
        <p role="status" className={ok ? "text-sm text-success" : "text-sm text-error"}>
          {result.status !== null ? `Test delivery returned ${result.status}.` : `Test delivery failed: ${result.error ?? "no response"}.`}
        </p>
      ) : null}
    </li>
  );
}
