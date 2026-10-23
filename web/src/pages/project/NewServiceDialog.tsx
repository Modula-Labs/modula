import * as Dialog from "@radix-ui/react-dialog";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { ArrowLeft, BrandDocker, ChevronRight, Database, Search } from "@/components/ui/icons";
import { Field, Input } from "@/components/ui/Input";
import { Alert } from "@/components/ui/misc";
import { ApiError, api } from "@/lib/api";

/** `ghcr.io/acme/api:1.2` → `api`, made a valid service name. */
export function serviceNameFromImage(image: string): string {
  const last = image.split("@")[0]?.split("/").pop() ?? "";
  const name = (last.split(":")[0] ?? "")
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/^[^a-z]+/, "")
    .replace(/-+$/, "")
    .slice(0, 63);
  return name || "service";
}

type Step = "root" | "image";

/** Command-style picker for adding a service (Docker image for now). */
export function NewServiceDialog({
  open,
  onOpenChange,
  projectId,
  environmentId,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  projectId: string;
  environmentId: string;
  onCreated: (serviceId: string) => void;
}) {
  const qc = useQueryClient();
  const [step, setStep] = useState<Step>("root");
  const [image, setImage] = useState("");
  const [name, setName] = useState("");
  const [port, setPort] = useState("8080");
  const [notice, setNotice] = useState<string | null>(null);

  const reset = () => {
    setStep("root");
    setImage("");
    setName("");
    setPort("8080");
    setNotice(null);
  };

  const create = useMutation({
    mutationFn: async () => {
      const svc = await api.createService(projectId, {
        name: name || serviceNameFromImage(image),
        config: { source: { type: "image", image: image.trim() }, port: Number(port) || 8080 },
      });
      const instance = svc.instances.find((i) => i.environmentId === environmentId);
      // Deploy right away, like Railway; without a server the service just waits.
      if (instance) {
        try {
          await api.deploy(instance.id);
        } catch (e) {
          if (!(e instanceof ApiError && e.status === 409)) throw e;
          setNotice(e.message);
        }
      }
      return svc;
    },
    onSuccess: async (svc) => {
      await qc.invalidateQueries({ queryKey: ["project", projectId] });
      onCreated(svc.id);
      onOpenChange(false);
      reset();
    },
  });

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) reset();
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50" />
        <Dialog.Content className="anim-modal fixed top-[16%] left-1/2 z-50 w-[480px] max-w-[calc(100vw-32px)] -translate-x-1/2 overflow-hidden rounded-xl bg-popover shadow-modal">
          <Dialog.Title className="sr-only">Add a service</Dialog.Title>
          <Dialog.Description className="sr-only">Choose where the service comes from.</Dialog.Description>
          <div className="flex h-12 items-center gap-2 border-b border-border px-3 text-muted-foreground">
            {step === "root" ? (
              <Search className="size-4" />
            ) : (
              <button aria-label="Back" onClick={() => setStep("root")} className="rounded p-0.5 hover:text-foreground">
                <ArrowLeft className="size-4" />
              </button>
            )}
            <span className="text-sm text-foreground">{step === "root" ? "What would you like to add?" : "Docker image"}</span>
          </div>

          {step === "root" ? (
            <div className="p-1.5">
              <PickerRow icon={<BrandDocker />} label="Docker image" onClick={() => setStep("image")} />
              <PickerRow icon={<Database />} label="Database" hint="Coming soon" />
            </div>
          ) : (
            <form
              className="flex flex-col gap-4 p-4"
              onSubmit={(e) => {
                e.preventDefault();
                create.mutate();
              }}
            >
              {create.error ? <Alert>{create.error.message}</Alert> : null}
              {notice ? <Alert tone="warn">{notice}</Alert> : null}
              <Field label="Image" hint="From Docker Hub, ghcr.io, quay.io or any registry, e.g. nginx:1.27">
                {(p) => (
                  <Input
                    {...p}
                    mono
                    autoFocus
                    required
                    placeholder="nginx:1.27"
                    value={image}
                    onChange={(e) => setImage(e.target.value)}
                  />
                )}
              </Field>
              <div className="grid grid-cols-[1fr_120px] gap-3">
                <Field label="Service name">
                  {(p) => (
                    <Input
                      {...p}
                      placeholder={image ? serviceNameFromImage(image) : "web"}
                      value={name}
                      onChange={(e) => setName(e.target.value)}
                    />
                  )}
                </Field>
                <Field label="Port">
                  {(p) => (
                    <Input {...p} inputMode="numeric" value={port} onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))} />
                  )}
                </Field>
              </div>
              <div className="flex justify-end gap-2">
                <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
                  Cancel
                </Button>
                <Button type="submit" variant="primary" loading={create.isPending} disabled={!image.trim()}>
                  Deploy
                </Button>
              </div>
            </form>
          )}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function PickerRow({
  icon,
  label,
  hint,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  hint?: string;
  onClick?: () => void;
}) {
  return (
    <button
      disabled={!onClick}
      onClick={onClick}
      className="flex h-11 w-full items-center gap-3 rounded-lg px-3 text-left text-sm hover:bg-accent disabled:cursor-default disabled:opacity-50 disabled:hover:bg-transparent [&>svg]:size-4 [&>svg]:text-muted-foreground"
    >
      {icon}
      <span className="flex-1">{label}</span>
      {hint ? <span className="text-xs text-muted-foreground">{hint}</span> : <ChevronRight />}
    </button>
  );
}
