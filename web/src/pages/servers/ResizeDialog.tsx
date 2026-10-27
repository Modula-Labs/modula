import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { clsx } from "clsx";
import { useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { MODAL_WIDTHS, Modal } from "@/components/ui/Modal";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import {
  api,
  type NodeInfo,
  type ResizeOptions,
  type ResizeStatus,
  type ResizeStep,
  type ResizeTypeOption,
} from "@/lib/api";
import { euros } from "@/lib/provision";

const STEPS: { step: ResizeStep; label: string }[] = [
  { step: "shutdown", label: "Shutting down" },
  { step: "change_type", label: "Changing the size" },
  { step: "poweron", label: "Starting again" },
  { step: "reconnect", label: "Waiting for the server to reconnect" },
];

/** Which of STEPS a step belongs to (a forced power-off is still shutting down). */
const stepIndex = (s: ResizeStep | null) =>
  s === "poweroff" ? 0 : STEPS.findIndex((x) => x.step === s);

const gb = (v: number) => (Number.isInteger(v) ? `${v}` : v.toFixed(1));

/**
 * Change a server's size in place at its cloud provider. Lists the sizes with
 * vCPU, memory, disk and price; sizes not sold in the server's location are
 * disabled with where they are sold, and `onMigrate` (when given) offers
 * moving the server there instead. The main server can't restart itself, so
 * for it this shows the CLI command. While a resize runs it shows progress.
 */
export function ResizeDialog({
  open,
  onOpenChange,
  nodeId,
  node,
  onMigrate,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  nodeId: string;
  node?: Pick<NodeInfo, "name"> | null;
  /** Offer "Change location instead" for a size sold elsewhere. */
  onMigrate?: (serverType: string) => void;
}) {
  const qc = useQueryClient();
  const options = useQuery({
    queryKey: ["resize-options", nodeId],
    queryFn: () => api.nodeResizeOptions(nodeId),
    enabled: open,
    staleTime: 30_000,
  });
  const [selected, setSelected] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [watching, setWatching] = useState(false);

  useEffect(() => {
    if (!open) {
      setSelected(null);
      setConfirming(false);
      setWatching(false);
    }
  }, [open]);

  const resizing = options.data?.resize.status === "resizing";
  const progress = useQuery({
    queryKey: ["resize-status", nodeId],
    queryFn: () => api.nodeResizeStatus(nodeId),
    enabled: open && (watching || resizing),
    refetchInterval: (q) =>
      q.state.data && q.state.data.status !== "resizing" ? false : 3000,
  });
  const done = watching && progress.data && progress.data.status !== "resizing";
  useEffect(() => {
    if (done) {
      void qc.invalidateQueries({ queryKey: ["resize-options", nodeId] });
      void qc.invalidateQueries({ queryKey: ["nodes"] });
    }
  }, [done, qc, nodeId]);

  const start = useMutation({
    mutationFn: (serverType: string) => api.resizeNode(nodeId, serverType),
    onSuccess: (s) => {
      qc.setQueryData(["resize-status", nodeId], s);
      setWatching(true);
      setConfirming(false);
    },
  });

  const name = node?.name ?? "this server";
  const data = options.data;
  const showProgress = watching || resizing;

  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      width={MODAL_WIDTHS.md}
      title={showProgress ? `Resizing ${name}` : `Resize ${name}`}
      description={
        showProgress
          ? "The server is restarted at its new size; what runs on it is back once it reconnects."
          : "Change the server's size in place at its provider: same disk, IP and data. It restarts, so what runs on it is down for a few minutes."
      }
    >
      {options.isPending ? (
        <Skeleton className="h-48" />
      ) : options.error ? (
        <Alert title="Couldn't load the sizes">{options.error.message}</Alert>
      ) : !data ? null : showProgress ? (
        <Progress
          status={progress.data ?? data.resize}
          onClose={() => onOpenChange(false)}
        />
      ) : !data.managed ? (
        <Unmanaged data={data} onClose={() => onOpenChange(false)} />
      ) : (
        <Picker
          data={data}
          selected={selected}
          onSelect={(n) => {
            setSelected(n);
            setConfirming(false);
          }}
          confirming={confirming}
          onConfirm={() => setConfirming(true)}
          onBack={() => setConfirming(false)}
          onStart={() => selected && start.mutate(selected)}
          starting={start.isPending}
          error={start.error?.message ?? null}
          onMigrate={onMigrate}
          onClose={() => onOpenChange(false)}
        />
      )}
    </Modal>
  );
}

function Unmanaged({ data, onClose }: { data: ResizeOptions; onClose: () => void }) {
  return (
    <div className="flex flex-col gap-4">
      <Alert tone="info" title="Can't resize this server from here">
        {data.reason}
      </Alert>
      {data.main && data.cliCommand ? <CliCommand command={data.cliCommand} /> : null}
      <div className="flex justify-end">
        <Button variant="secondary" onClick={onClose}>
          Close
        </Button>
      </div>
    </div>
  );
}

function CliCommand({ command }: { command: string }) {
  return (
    <div className="flex flex-col gap-1.5">
      <p className="text-[13px] text-muted-foreground">
        This server runs the dashboard and can't switch itself off and back on.
        Resize it from your computer with the modula CLI:
      </p>
      <div className="flex items-center gap-2 rounded-lg border border-border bg-muted p-3">
        <code className="min-w-0 flex-1 font-mono text-[13px] leading-5 break-all">{command}</code>
        <CopyButton value={command} label="Copy command" />
      </div>
    </div>
  );
}

function Picker({
  data,
  selected,
  onSelect,
  confirming,
  onConfirm,
  onBack,
  onStart,
  starting,
  error,
  onMigrate,
  onClose,
}: {
  data: ResizeOptions;
  selected: string | null;
  onSelect: (name: string) => void;
  confirming: boolean;
  onConfirm: () => void;
  onBack: () => void;
  onStart: () => void;
  starting: boolean;
  error: string | null;
  onMigrate?: (serverType: string) => void;
  onClose: () => void;
}) {
  const current = data.current;
  const chosen = data.types.find((t) => t.name === selected) ?? null;
  // Usable ones first, then sold elsewhere, then the ones that can't be used.
  const types = useMemo(() => {
    const rank = (t: ResizeTypeOption) =>
      t.current ? 0 : t.availableHere && t.compatible ? 1 : t.compatible ? 2 : 3;
    return [...data.types].sort((a, b) => rank(a) - rank(b));
  }, [data.types]);
  const command = data.cliCommand
    ? `${data.cliCommand}${selected ? ` --type ${selected}` : ""}`
    : null;

  return (
    <div className="flex flex-col gap-4">
      {current ? (
        <p className="text-[13px] text-muted-foreground">
          Now <span className="font-mono text-foreground">{current.name}</span>
          {" · "}
          {current.cores} vCPU · {gb(current.memoryGb)} GB RAM · {current.diskGb} GB disk
          {current.monthlyCents != null ? ` · ${euros(current.monthlyCents)}/mo` : ""}
          {data.location ? ` · ${data.location.city}` : ""}
        </p>
      ) : null}
      {data.resize.error ? (
        <Alert tone="warn" title="The last resize didn't finish">
          {data.resize.error}
        </Alert>
      ) : null}

      <div
        role="radiogroup"
        aria-label="Server size"
        className="flex max-h-[340px] flex-col overflow-y-auto rounded-lg border border-border"
      >
        {types.map((t) => (
          <TypeRow
            key={t.name}
            t={t}
            selected={t.name === selected}
            onSelect={() => onSelect(t.name)}
            onMigrate={onMigrate}
          />
        ))}
      </div>
      <p className="text-[13px] text-muted-foreground">
        The disk keeps its size, so the server can go back to a smaller size later.
        Sizes with a smaller disk or another CPU architecture can't be used.
      </p>

      {error ? <Alert>{error}</Alert> : null}

      {data.main ? (
        <>
          {command ? <CliCommand command={command} /> : null}
          <div className="flex justify-end">
            <Button variant="secondary" onClick={onClose}>
              Close
            </Button>
          </div>
        </>
      ) : confirming && chosen ? (
        <div className="flex flex-col gap-3 border-t border-border pt-4">
          <Alert tone="warn" title={`Resize to ${chosen.name}?`}>
            The server shuts down, changes size and starts again. Everything on it
            is unavailable for a few minutes.
            {chosen.monthlyCents != null && current?.monthlyCents != null
              ? ` ${euros(current.monthlyCents)}/mo → ${euros(chosen.monthlyCents)}/mo.`
              : ""}
          </Alert>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onBack}>
              Back
            </Button>
            <Button variant="primary" loading={starting} onClick={onStart}>
              Resize now
            </Button>
          </div>
        </div>
      ) : (
        <div className="flex justify-end gap-2 border-t border-border pt-4">
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant="primary"
            disabled={!chosen || chosen.current || !chosen.availableHere || !chosen.compatible}
            onClick={onConfirm}
          >
            Resize
          </Button>
        </div>
      )}
    </div>
  );
}

function TypeRow({
  t,
  selected,
  onSelect,
  onMigrate,
}: {
  t: ResizeTypeOption;
  selected: boolean;
  onSelect: () => void;
  onMigrate?: (serverType: string) => void;
}) {
  const usable = !t.current && t.availableHere && t.compatible;
  const elsewhere = !t.availableHere && t.compatible && t.availableIn.length > 0;
  return (
    <div
      className={clsx(
        "flex items-center gap-3 border-b border-border px-3 py-2 last:border-b-0",
        selected && "bg-primary/10",
        !usable && !t.current && "text-muted-foreground",
      )}
    >
      <button
        type="button"
        role="radio"
        aria-checked={selected}
        disabled={!usable}
        onClick={onSelect}
        className="flex min-w-0 flex-1 items-center gap-3 text-left disabled:cursor-not-allowed"
      >
        <span
          aria-hidden
          className={clsx(
            "size-3.5 shrink-0 rounded-full border",
            selected ? "border-primary bg-primary" : "border-input",
            !usable && "opacity-40",
          )}
        />
        <span className="w-16 shrink-0 font-mono text-sm">{t.name}</span>
        <span className="min-w-0 flex-1 text-[13px]">
          {t.cores} vCPU · {gb(t.memoryGb)} GB · {t.diskGb} GB disk
          {t.arch === "arm" ? " · Arm" : ""}
          {t.blocker ? (
            <span className="block truncate text-xs" title={t.blocker}>
              {t.blocker}
            </span>
          ) : elsewhere ? (
            <span className="block truncate text-xs">
              Not sold here · available in{" "}
              {t.availableIn.map((l) => l.city).join(", ")}
            </span>
          ) : null}
        </span>
      </button>
      {t.current ? <Badge>Current</Badge> : null}
      {elsewhere && onMigrate ? (
        <Button size="xs" variant="outline" onClick={() => onMigrate(t.name)}>
          Change location instead
        </Button>
      ) : null}
      <span className="w-20 shrink-0 text-right text-[13px] tabular-nums">
        {t.monthlyCents != null ? `${euros(t.monthlyCents)}/mo` : "—"}
        {usable && t.direction !== "same" ? (
          <span className="ml-1 text-muted-foreground">{t.direction === "up" ? "↑" : "↓"}</span>
        ) : null}
      </span>
    </div>
  );
}

function Progress({ status, onClose }: { status: ResizeStatus; onClose: () => void }) {
  const running = status.status === "resizing";
  const at = stepIndex(status.step);
  return (
    <div className="flex flex-col gap-4">
      <p className="text-[13px] text-muted-foreground">
        {status.from && status.to ? (
          <>
            <span className="font-mono text-foreground">{status.from}</span> →{" "}
            <span className="font-mono text-foreground">{status.to}</span>
          </>
        ) : null}
      </p>
      <ol className="flex flex-col gap-2">
        {STEPS.map((s, i) => {
          const state = !running ? (status.error ? "idle" : "done") : i < at ? "done" : i === at ? "active" : "idle";
          return (
            <li key={s.step} className="flex items-center gap-2 text-sm">
              <span
                aria-hidden
                className={clsx(
                  "size-2 rounded-full",
                  state === "done" && "bg-primary",
                  state === "active" && "animate-pulse bg-primary",
                  state === "idle" && "bg-foreground/20",
                )}
              />
              <span className={clsx(state === "idle" && "text-muted-foreground")}>
                {s.label}
                {s.step === "shutdown" && status.step === "poweroff" && running
                  ? " (cutting the power: it didn't stop in time)"
                  : ""}
              </span>
            </li>
          );
        })}
      </ol>
      {!running && status.error ? (
        <Alert title="The resize didn't finish">{status.error}</Alert>
      ) : !running ? (
        <Alert tone="success" title="Resized">
          The server runs as {status.to ?? "its new size"} now.
        </Alert>
      ) : null}
      <div className="flex justify-end">
        <Button variant="secondary" onClick={onClose}>
          {running ? "Close (continues in the background)" : "Done"}
        </Button>
      </div>
    </div>
  );
}
