import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/Tabs";
import { api, type Instance } from "@/lib/api";
import { ConnectRow } from "../ConnectDialog";

function CodeBox({ value, label }: { value: string; label: string }) {
  return (
    <div className="flex items-center gap-1 rounded-md border border-border bg-background py-1 pr-1 pl-3">
      <code className="min-w-0 flex-1 truncate font-mono text-[12.5px]" title={value}>
        {value}
      </code>
      <CopyButton value={value} label={label} />
    </div>
  );
}

function Step({ n, children }: { n: number; children: ReactNode }) {
  return (
    <li className="flex gap-3">
      <span className="flex size-6 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-medium text-muted-foreground">{n}</span>
      <div className="flex min-w-0 flex-1 flex-col gap-2 pt-0.5 text-sm">{children}</div>
    </li>
  );
}

export function ConnectPostgres({
  open,
  onOpenChange,
  instance,
  canEdit,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  instance: Instance;
  canEdit: boolean;
}) {
  const qc = useQueryClient();
  const [tab, setTab] = useState<"private" | "public">("private");
  const [cliHelp, setCliHelp] = useState(false);
  const name = instance.serviceName;
  const proxy = instance.tcpProxy;
  const vars = useQuery({ queryKey: ["variables", instance.id], queryFn: () => api.variables(instance.id), enabled: open && tab === "public" && !!proxy });
  const enable = useMutation({
    mutationFn: () => api.createTcpProxy(instance.id, instance.config.port),
    onSuccess: (i) => {
      qc.setQueryData(["instance", instance.id], i);
      void qc.invalidateQueries({ queryKey: ["variables", instance.id] });
    },
  });
  const connect = `modula-deploy connect ${name} -e ${instance.environmentName}`;

  return (
    <Modal open={open} onOpenChange={onOpenChange} width={600} title={`Connect to ${name}`}>
      <div className="flex flex-col gap-5">
        <Segmented
          label="Network"
          value={tab}
          onChange={setTab}
          options={[
            { value: "private", label: "Private Network" },
            { value: "public", label: "Public Network" },
          ]}
        />
        {tab === "private" ? (
          <ol className="flex flex-col gap-4">
            <Step n={1}>
              <span>Create a new variable in the service you want to connect to this database.</span>
            </Step>
            <Step n={2}>
              <span>Assign it the following value:</span>
              <CodeBox value={`\${{${name}.DATABASE_URL}}`} label="Copy reference" />
            </Step>
            <Step n={3}>
              <span>Use the variable in your application code.</span>
            </Step>
          </ol>
        ) : (
          <div className="flex flex-col gap-5">
            <p className="text-sm text-muted-foreground">Private by default, only services inside your project can reach {name}.</p>
            <div className="flex flex-col gap-2 rounded-lg border border-border p-3">
              <div>
                <div className="text-sm font-medium">Public Access</div>
                <p className="text-sm text-muted-foreground">
                  Adds a public host and port and sets <code className="font-mono text-[12.5px]">DATABASE_PUBLIC_URL</code>
                </p>
              </div>
              <Alert tone="danger">Anyone with the string can connect, use a strong password and SSL.</Alert>
              {proxy ? (
                <div className="flex flex-col gap-2">
                  <div className="flex items-center justify-between gap-2 rounded-md border border-border px-3 py-2 text-sm">
                    <span className="text-muted-foreground">TCP proxy</span>
                    <span className="font-mono text-[13px]">
                      {proxy.host}:{proxy.publicPort}
                    </span>
                  </div>
                  {vars.data ? (
                    <div className="rounded-lg border border-border">
                      <ConnectRow
                        label="Public URL"
                        variable="DATABASE_PUBLIC_URL"
                        reference={`\${{${name}.DATABASE_PUBLIC_URL}}`}
                        info={vars.data.find((v) => v.name === "DATABASE_PUBLIC_URL")}
                      />
                    </div>
                  ) : null}
                  <p className="text-xs text-muted-foreground">Remove it any time from Networking settings.</p>
                </div>
              ) : null}
            </div>
            <div className="flex flex-col gap-3">
              <h4 className="text-sm font-semibold">Connect without exposing the database</h4>
              <div className="grid grid-cols-1 gap-3 md:grid-cols-2">
                <div className="flex flex-col gap-2 rounded-lg border border-border p-3">
                  <div className="text-sm font-medium">From your machine</div>
                  <p className="text-xs text-muted-foreground">Opens an encrypted tunnel session with no public access needed.</p>
                  <CodeBox value={connect} label="Copy command" />
                  <button type="button" className="self-start text-xs text-muted-foreground underline underline-offset-2 hover:text-foreground" onClick={() => setCliHelp((s) => !s)}>
                    Install the CLI
                  </button>
                  {cliHelp ? <p className="text-xs text-muted-foreground">Install modula-deploy on your machine, then run `modula-deploy login` once.</p> : null}
                </div>
                <div className="flex flex-col gap-2 rounded-lg border border-border p-3">
                  <div className="text-sm font-medium">External GUI client</div>
                  <p className="text-xs text-muted-foreground">Keeps the tunnel open for tools like TablePlus, DBeaver, or pgAdmin.</p>
                  <CodeBox value={`${connect} --tunnel-only`} label="Copy command" />
                </div>
              </div>
            </div>
            {enable.error ? <Alert>{enable.error.message}</Alert> : null}
            {!proxy ? (
              canEdit ? (
                <div className="flex justify-end">
                  <Button variant="primary" loading={enable.isPending} onClick={() => enable.mutate()}>
                    Add Public Access
                  </Button>
                </div>
              ) : (
                <p className="text-xs text-muted-foreground">Ask a member to enable public access.</p>
              )
            ) : null}
          </div>
        )}
      </div>
    </Modal>
  );
}
