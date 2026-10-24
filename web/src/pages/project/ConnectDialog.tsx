import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Check, Copy, Eye, EyeOff } from "@/components/ui/icons";
import { Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert, Skeleton } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import { api, type Instance, type Template, type VariableInfo } from "@/lib/api";
import { ConnectPostgres } from "./database/ConnectPostgres";

/** Public entries use the TCP proxy; everything else stays on the private network. */
export const isPublicEntry = (e: { label: string; variable: string }) => /public/i.test(e.label) || /public/i.test(e.variable);

export function ConnectDialog(props: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  instance: Instance;
  template: Template | undefined;
  canEdit: boolean;
}) {
  if (props.instance.template === "postgres") return <ConnectPostgres {...props} />;
  return <GenericConnectDialog {...props} />;
}

function GenericConnectDialog({
  open,
  onOpenChange,
  instance,
  template,
  canEdit,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  instance: Instance;
  template: Template | undefined;
  canEdit: boolean;
}) {
  const qc = useQueryClient();
  const [tab, setTab] = useState<"private" | "public">("private");
  const [port, setPort] = useState(String(instance.config.port));
  const vars = useQuery({ queryKey: ["variables", instance.id], queryFn: () => api.variables(instance.id), enabled: open });
  const enable = useMutation({
    mutationFn: () => api.createTcpProxy(instance.id, Number(port)),
    onSuccess: (i) => {
      qc.setQueryData(["instance", instance.id], i);
      void qc.invalidateQueries({ queryKey: ["variables", instance.id] });
    },
  });
  const entries = (template?.connect ?? []).filter((e) => (tab === "public") === isPublicEntry(e));
  const proxy = instance.tcpProxy;

  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      width={560}
      title={`Connect to ${instance.serviceName}`}
      description="Use these from other services, or from a client on your machine."
    >
      <div className="flex flex-col gap-4">
        <Segmented
          label="Network"
          value={tab}
          onChange={setTab}
          options={[
            { value: "private", label: "Private network" },
            { value: "public", label: "Public network" },
          ]}
        />
        {tab === "public" && !proxy ? (
          <div className="flex flex-col gap-3 rounded-xl border border-border p-4">
            <p className="text-sm text-muted-foreground">
              Public access goes through a TCP proxy. Anyone with the credentials can reach the database from the internet, so enable it only when you need it.
            </p>
            {enable.error ? <Alert>{enable.error.message}</Alert> : null}
            {canEdit ? (
              <div className="flex items-end gap-2">
                <label className="flex flex-col gap-1.5 text-sm font-bold">
                  Target port
                  <Input className="w-28" inputMode="numeric" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
                </label>
                <Button variant="primary" loading={enable.isPending} disabled={!Number(port)} onClick={() => enable.mutate()}>
                  Enable TCP proxy
                </Button>
              </div>
            ) : (
              <p className="text-xs text-muted-foreground">Ask a member to enable it.</p>
            )}
          </div>
        ) : (
          <>
            {tab === "public" && proxy ? (
              <div className="flex items-center justify-between gap-2 rounded-lg border border-border px-3 py-2.5 text-sm">
                <span className="text-muted-foreground">TCP proxy</span>
                <span className="font-mono text-[13px]">
                  {proxy.host}:{proxy.publicPort}
                </span>
              </div>
            ) : null}
            {vars.isPending ? (
              <Skeleton className="h-24" />
            ) : vars.error ? (
              <Alert>{vars.error.message}</Alert>
            ) : entries.length === 0 ? (
              <p className="text-sm text-muted-foreground">Nothing to show here.</p>
            ) : (
              <div className="divide-y divide-border rounded-xl border border-border">
                {entries.map((e) => (
                  <ConnectRow
                    key={e.variable}
                    label={e.label}
                    variable={e.variable}
                    reference={`\${{${instance.serviceName}.${e.variable}}}`}
                    info={vars.data?.find((v) => v.name === e.variable)}
                  />
                ))}
              </div>
            )}
          </>
        )}
      </div>
    </Modal>
  );
}

export function ConnectRow({ label, variable, reference, info }: { label: string; variable: string; reference: string; info?: VariableInfo }) {
  const [value, setValue] = useState<string | null>(null);
  const [shown, setShown] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchValue = async () => {
    if (value !== null) return value;
    if (!info) throw new Error("The variable isn't set yet. Deploy the service first.");
    const v = (await api.revealVariable(info.id)).value;
    setValue(v);
    return v;
  };
  const run = async (f: () => Promise<void>) => {
    setError(null);
    try {
      await f();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Something went wrong.");
    }
  };

  return (
    <div className="flex flex-col gap-1.5 px-3 py-2.5">
      <div className="flex items-center justify-between gap-2">
        <span className="text-sm font-bold">{label}</span>
        <span className="truncate font-mono text-xs text-muted-foreground" title={variable}>
          {reference}
        </span>
      </div>
      <div className="flex items-center gap-1">
        <code className="min-w-0 flex-1 truncate rounded-lg bg-muted px-2 py-1.5 font-mono text-[13px]">
          {shown && value !== null ? value : "•".repeat(24)}
        </code>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          aria-label={shown ? `Hide ${label}` : `Reveal ${label}`}
          disabled={!info}
          className="text-muted-foreground"
          onClick={() =>
            run(async () => {
              if (shown) return setShown(false);
              await fetchValue();
              setShown(true);
            })
          }
        >
          {shown ? <EyeOff /> : <Eye />}
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          aria-label={`Copy ${label}`}
          disabled={!info}
          className="text-muted-foreground"
          onClick={() =>
            run(async () => {
              await navigator.clipboard.writeText(await fetchValue());
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            })
          }
        >
          {copied ? <Check /> : <Copy />}
        </Button>
      </div>
      {error ? <p className="text-xs text-error">{error}</p> : null}
    </div>
  );
}
