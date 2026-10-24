import { useQuery } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import type { Dispatch, SetStateAction } from "react";
import { Field, Input } from "@/components/ui/Input";
import { Segmented } from "@/components/ui/Tabs";
import { defaultScriptName, validateOutputDir, validateScriptName, workersDevHost } from "@/lib/hosting";
import { cloudflare } from "@/lib/integrations";
import type { Form } from "./SettingsTab";

/** Where a static site is served from: your servers or a Cloudflare Worker. Edits the settings form. */
export function HostingFields({
  form,
  setForm,
  canEdit,
  workspaceId,
  serviceName,
  environmentName,
}: {
  form: Form;
  setForm: Dispatch<SetStateAction<Form>>;
  canEdit: boolean;
  workspaceId: string | undefined;
  serviceName: string;
  environmentName: string;
}) {
  const status = useQuery({
    queryKey: ["cloudflare", workspaceId],
    queryFn: () => cloudflare.status(workspaceId!),
    enabled: !!workspaceId,
  });
  const connected = status.data?.connected === true;
  // A service already on Cloudflare keeps its choice visible even if the connection is gone.
  const canChoose = canEdit && (connected || form.cloudflare);
  const defaultName = defaultScriptName(serviceName, environmentName);
  const patch = (p: Partial<Form>) => setForm((f) => ({ ...f, ...p }));
  const choose = (v: "server" | "cloudflare") =>
    patch(v === "cloudflare" ? { cloudflare: true, outputDir: form.outputDir || "dist" } : { cloudflare: false });
  const outputError = form.cloudflare && form.outputDir ? validateOutputDir(form.outputDir) : null;
  const nameError = validateScriptName(form.scriptName);

  return (
    <>
      <div className="flex flex-col gap-1.5">
        <div className="text-sm leading-none font-normal">Host on</div>
        <div className={canChoose ? undefined : "pointer-events-none opacity-50"}>
          <Segmented
            label="Host on"
            value={form.cloudflare ? "cloudflare" : "server"}
            onChange={choose}
            options={[
              { value: "server", label: "Your servers" },
              { value: "cloudflare", label: "Cloudflare" },
            ]}
          />
        </div>
        <p className="text-xs text-muted-foreground">
          {form.cloudflare
            ? "Built on one of your servers, then the output is uploaded to Cloudflare as a Worker with static assets."
            : "Runs as a container on your servers."}
          {!connected && !status.isPending && !form.cloudflare ? (
            <>
              {" "}
              Cloudflare isn't connected.{" "}
              <Link to="/settings" className="underline">
                Connect it in Settings → Integrations
              </Link>
              .
            </>
          ) : null}
        </p>
      </div>
      {form.cloudflare ? (
        <>
          <Field label="Output directory" hint="Where the build writes the site, relative to the root directory." error={outputError}>
            {(p) => <Input {...p} mono disabled={!canEdit} placeholder="dist" value={form.outputDir} onChange={(e) => patch({ outputDir: e.target.value })} />}
          </Field>
          <label className="flex items-start gap-2.5 text-sm">
            <input type="checkbox" disabled={!canEdit} checked={form.spa} onChange={(e) => patch({ spa: e.target.checked })} className="mt-0.5 size-4 accent-foreground" />
            <span>
              <span className="leading-none font-normal">Single-page app</span>
              <span className="block text-xs text-muted-foreground">Serve index.html for unknown paths.</span>
            </span>
          </label>
          <Field
            label="Worker name"
            hint={
              <>
                Served at <span className="font-mono">{workersDevHost(form.scriptName.trim() || defaultName)}</span> and on your custom domains.
              </>
            }
            error={nameError}
          >
            {(p) => <Input {...p} mono disabled={!canEdit} placeholder={defaultName} value={form.scriptName} onChange={(e) => patch({ scriptName: e.target.value })} />}
          </Field>
          <div className="text-xs text-muted-foreground">
            Variables starting with <span className="font-mono">VITE_</span>, <span className="font-mono">NEXT_PUBLIC_</span>, <span className="font-mono">PUBLIC_</span> and similar framework prefixes are used by the build only. Every other variable is set on the Worker at each deploy (sealed and credential-like ones as secrets). Variables you remove here are removed from the Worker.
          </div>
        </>
      ) : null}
    </>
  );
}
