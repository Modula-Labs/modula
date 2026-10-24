import { clsx } from "clsx";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { Button } from "@/components/ui/Button";
import { Checkbox } from "@/components/ui/Controls";
import { CopyButton } from "@/components/ui/CopyButton";
import { DotsVertical, Eye, EyeOff, FileText, Lock, Pencil, Plus, Trash, Variable } from "@/components/ui/icons";
import { Input, Textarea } from "@/components/ui/Input";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/Menu";
import { Alert, Badge, EmptyState, Skeleton } from "@/components/ui/misc";
import { ApiError, api, type VariableInfo, type VariableNames, type VariablePatch } from "@/lib/api";
import { diffEnv, envTemplate, parseDotenv } from "@/lib/dotenv";
import { relativeTime } from "@/lib/format";
import { referenceOptions, type ReferenceOption } from "@/lib/references";
import { ReferenceInput } from "./ReferenceInput";

/** Where a variables list lives: one service instance, or the environment's shared set. */
export interface VariablesScope {
  /** Query key of the list. */
  key: unknown[];
  environmentId: string;
  /** The service whose own names complete without a prefix; none for shared. */
  serviceId?: string;
  load: () => Promise<VariableInfo[]>;
  save: (b: VariablePatch) => Promise<VariableInfo[]>;
}

export function VariablesTab({
  instanceId,
  environmentId,
  serviceId,
  canEdit,
}: {
  instanceId: string;
  environmentId: string;
  serviceId: string;
  canEdit: boolean;
}) {
  const scope: VariablesScope = {
    key: ["variables", instanceId],
    environmentId,
    serviceId,
    load: () => api.variables(instanceId),
    save: (b) => api.updateVariables(instanceId, b),
  };
  return <VariablesEditor scope={scope} canEdit={canEdit} noun="service" />;
}

/** Write-only variables: values never come back from the API except through
 *  an audited, sudo-gated reveal, and sealed ones not even then. Used for a
 *  service's variables and for an environment's shared variables. */
export function VariablesEditor({ scope, canEdit, noun }: { scope: VariablesScope; canEdit: boolean; noun: "service" | "shared" }) {
  const qc = useQueryClient();
  const vars = useQuery({ queryKey: scope.key, queryFn: scope.load });
  const names = useQuery({
    queryKey: ["variable-names", scope.environmentId],
    queryFn: async (): Promise<VariableNames> => {
      try {
        return await api.variableNames(scope.environmentId);
      } catch (e) {
        // Older servers have no autocomplete endpoint.
        if (e instanceof ApiError && e.status === 404) return { shared: [], services: [] };
        throw e;
      }
    },
    staleTime: 30_000,
    retry: false,
  });
  const options = useMemo(() => (names.data ? referenceOptions(names.data, scope.serviceId) : []), [names.data, scope.serviceId]);
  const [adding, setAdding] = useState(false);
  const [raw, setRaw] = useState(false);
  // Sealed ones last: they're write-only, so there's less to do with them.
  const sorted = useMemo(() => (vars.data ? [...vars.data].sort((a, b) => Number(a.sealed) - Number(b.sealed)) : undefined), [vars.data]);
  const update = useMutation({
    mutationFn: scope.save,
    onSuccess: (list) => {
      qc.setQueryData(scope.key, list);
      // Edits are saved at once but only apply on the next deploy; references also change the canvas edges.
      void qc.invalidateQueries({ queryKey: ["pending", scope.environmentId] });
      void qc.invalidateQueries({ queryKey: ["variable-names", scope.environmentId] });
      void qc.invalidateQueries({ queryKey: ["project"] });
    },
  });

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <div className="text-sm text-muted-foreground">
          {vars.data ? `${vars.data.length} ${noun} variable${vars.data.length === 1 ? "" : "s"}` : " "}
        </div>
        {canEdit ? (
          <div className="flex gap-2">
            <Button
              variant="ghost"
              size="sm"
              icon={<FileText />}
              aria-pressed={raw}
              disabled={!vars.data}
              onClick={() => {
                setRaw((r) => !r);
                setAdding(false);
              }}
            >
              Raw editor
            </Button>
            <Button variant="secondary" size="sm" icon={<Plus />} disabled={raw} onClick={() => setAdding(true)}>
              New variable
            </Button>
          </div>
        ) : null}
      </div>
      <p className="text-sm text-muted-foreground">
        {noun === "shared" ? "Available to every service in this environment as " : "Changes apply on the next deploy. Reference other variables with "}
        <span className="font-mono">{noun === "shared" ? "${{shared.NAME}}" : "${{service.NAME}}"}</span>
        {noun === "shared" ? ". Changes apply on the next deploy." : "."}
      </p>

      {update.error ? <Alert>{update.error.message}</Alert> : null}
      {raw && vars.data ? (
        <RawEditor
          variables={vars.data}
          busy={update.isPending}
          onSubmit={(patch) => update.mutateAsync(patch).then(() => setRaw(false))}
          onCancel={() => setRaw(false)}
        />
      ) : null}
      {adding && !raw ? (
        <NewVariableRow
          busy={update.isPending}
          options={options}
          onCancel={() => setAdding(false)}
          onSubmit={(name, value, sealed) =>
            update.mutateAsync({ set: { [name]: value }, sealed: sealed ? [name] : [] }).then(() => setAdding(false))
          }
        />
      ) : null}

      {raw ? null : vars.isPending ? (
        <Skeleton className="h-32" />
      ) : vars.data && vars.data.length === 0 && !adding ? (
        <EmptyState
          icon={<Variable />}
          title={noun === "shared" ? "No shared variables yet" : "No variables yet"}
          body="Variables are encrypted at rest and injected into the container on deploy."
        />
      ) : (
        <div className="divide-y divide-border overflow-hidden rounded-xl border border-border">
          {sorted?.map((v) => (
            <VariableRow
              key={v.id}
              v={v}
              canEdit={canEdit}
              options={options}
              onSet={(value) => update.mutateAsync({ set: { [v.name]: value } })}
              onDelete={() => update.mutate({ delete: [v.name] })}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function VariableRow({
  v,
  canEdit,
  options,
  onSet,
  onDelete,
}: {
  v: VariableInfo;
  canEdit: boolean;
  options: ReferenceOption[];
  onSet: (value: string) => Promise<unknown>;
  onDelete: () => void;
}) {
  const [revealed, setRevealed] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);

  const input = useRef<HTMLInputElement>(null);
  /** Edit in place: the current value (when revealable) goes in selected. */
  const edit = async () => {
    setError(null);
    let current = revealed ?? "";
    if (!v.sealed && revealed === null) {
      try {
        current = (await api.revealVariable(v.id)).value;
      } catch {
        // Not revealable right now (e.g. sudo declined): start empty.
      }
    }
    setValue(current);
    setEditing(true);
    requestAnimationFrame(() => input.current?.select());
  };

  const reveal = async () => {
    setError(null);
    try {
      setRevealed((await api.revealVariable(v.id)).value);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not reveal the value.");
    }
  };

  return (
    <div className="group flex min-h-12 items-center gap-3 px-4 py-2 transition-colors hover:bg-muted/50">
      <div className="flex w-[38%] min-w-0 flex-col gap-1">
        <div className="flex min-w-0 items-center gap-2">
          <span className="truncate font-mono text-[13px]">{v.name}</span>
          {v.sealed ? (
            <span title="Sealed: write-only, can never be revealed">
              <Lock className="size-3.5 shrink-0 text-muted-foreground" />
            </span>
          ) : null}
        </div>
        {v.references?.length ? (
          <div className="flex flex-wrap gap-1" aria-label="References">
            {v.references.map((r) => (
              <span key={r} title={`Refers to ${r}`} className="rounded-sm bg-muted px-1.5 font-mono text-xs leading-[18px] text-muted-foreground">
                {r}
              </span>
            ))}
          </div>
        ) : null}
      </div>
      <div className="flex min-w-0 flex-1 items-center gap-2">
        {editing ? (
          <form
            className="flex flex-1 gap-2"
            onSubmit={async (e) => {
              e.preventDefault();
              await onSet(value);
              setEditing(false);
              setRevealed(null);
              setValue("");
            }}
          >
            <ReferenceInput ref={input} mono autoFocus placeholder={v.sealed ? "New value (sealed)" : "New value"} aria-label={`New value for ${v.name}`} value={value} onValueChange={setValue} options={options} />
            <Button type="submit" size="sm" variant="secondary">
              Save
            </Button>
            <Button type="button" size="sm" variant="ghost" onClick={() => setEditing(false)}>
              Cancel
            </Button>
          </form>
        ) : revealed !== null ? (
          <>
            <span className="truncate font-mono text-[13px]">{revealed}</span>
            <CopyButton value={revealed} />
            <Button aria-label="Hide value" variant="ghost" size="icon-sm" className="text-muted-foreground" onClick={() => setRevealed(null)}>
              <EyeOff />
            </Button>
          </>
        ) : (
          <span
            className={clsx("font-mono text-[13px] tracking-widest text-muted-foreground", canEdit && "cursor-text select-none")}
            title={canEdit ? "Double-click to edit" : undefined}
            onDoubleClick={canEdit ? () => void edit() : undefined}
          >
            *******
          </span>
        )}
        {error ? <span className="text-[13px] text-error">{error}</span> : null}
      </div>
      {!editing ? <span className="hidden text-[13px] text-muted-foreground sm:block">{relativeTime(v.updatedAt)}</span> : null}
      {v.sealed ? <Badge>Sealed</Badge> : null}
      {canEdit && !editing ? (
        <Menu
          trigger={
            <Button aria-label={`Actions for ${v.name}`} variant="ghost" size="icon-sm" className="text-muted-foreground">
              <DotsVertical />
            </Button>
          }
        >
          {!v.sealed ? (
            <MenuItem icon={<Eye />} onSelect={() => void reveal()}>
              Reveal value
            </MenuItem>
          ) : null}
          <MenuItem icon={<Pencil />} onSelect={() => void edit()}>
            Edit
          </MenuItem>
          <MenuSeparator />
          <MenuItem danger icon={<Trash />} onSelect={onDelete}>
            Delete
          </MenuItem>
        </Menu>
      ) : null}
    </div>
  );
}

function NewVariableRow({
  busy,
  options,
  onCancel,
  onSubmit,
}: {
  busy: boolean;
  options: ReferenceOption[];
  onCancel: () => void;
  onSubmit: (name: string, value: string, sealed: boolean) => Promise<unknown>;
}) {
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [sealed, setSealed] = useState(false);
  return (
    <form
      className="flex flex-col gap-3 rounded-xl border border-border p-4"
      onSubmit={(e) => {
        e.preventDefault();
        void onSubmit(name.trim(), value, sealed);
      }}
    >
      <div className="grid grid-cols-[38%_1fr] gap-3">
        <Input mono autoFocus required aria-label="Variable name" placeholder="VARIABLE_NAME" value={name} onChange={(e) => setName(e.target.value.replace(/\s/g, "_"))} />
        <ReferenceInput mono aria-label="Value" placeholder="value, or ${{service.NAME}}" value={value} onValueChange={setValue} options={options} />
      </div>
      <div className="flex items-center justify-between">
        <Checkbox
          checked={sealed}
          onCheckedChange={setSealed}
          label="Seal: never revealable, even to admins"
          className="text-muted-foreground"
        />
        <div className="flex gap-2">
          <Button type="button" size="sm" variant="ghost" onClick={onCancel}>
            Cancel
          </Button>
          <Button type="submit" size="sm" variant="secondary" loading={busy} disabled={!name.trim()}>
            Add
          </Button>
        </div>
      </div>
    </form>
  );
}

/** `.env` view of the whole list. Values are write-only (the API never returns
 *  them), so it starts from bare `NAME=` lines and works as a diff: an existing
 *  name with an empty value is left untouched, a new name or a typed value is
 *  sent, and a deleted line deletes the variable. Sealed variables look the
 *  same as the others and are only ever replaced, never shown. */
function RawEditor({
  variables,
  busy,
  onSubmit,
  onCancel,
}: {
  variables: VariableInfo[];
  busy: boolean;
  onSubmit: (patch: VariablePatch) => Promise<unknown>;
  onCancel: () => void;
}) {
  // Sealed variables stay out: they can't be shown, and leaving them out
  // can't delete them (the diff only covers the names listed here).
  const existing = variables.filter((v) => !v.sealed).map((v) => v.name);
  const sealed = variables.length - existing.length;
  // Current values, revealed (audited, sudo-gated) when the editor opens.
  const [values, setValues] = useState<Record<string, string> | null>(null);
  const [text, setText] = useState("");
  useEffect(() => {
    let off = false;
    void (async () => {
      const open = variables.filter((v) => !v.sealed);
      const out: Record<string, string> = {};
      try {
        // One first, so a sudo prompt shows once; the rest ride on it.
        if (open[0]) out[open[0].name] = (await api.revealVariable(open[0].id)).value;
        const rest = await Promise.allSettled(open.slice(1).map((v) => api.revealVariable(v.id)));
        rest.forEach((r, i) => {
          if (r.status === "fulfilled") out[open[i + 1]!.name] = r.value.value;
        });
      } catch {
        // Not revealable now: fall back to bare NAME= lines.
      }
      if (off) return;
      setValues(out);
      setText(envTemplate(existing, out));
    })();
    return () => {
      off = true;
    };
    // Once per opening: the list it was opened with is what it edits.
  }, []); // eslint-disable-line react-hooks/exhaustive-deps
  const parsed = parseDotenv(text);
  const diff = diffEnv(existing, parsed.vars, values ?? {});
  const hidden = existing.filter((n) => !(values && n in values)).length;
  const total = Object.keys(diff.set).length + diff.delete.length;
  const summary = [
    diff.added.length ? `${diff.added.length} added` : "",
    diff.changed.length ? `${diff.changed.length} updated` : "",
    diff.delete.length ? `${diff.delete.length} removed` : "",
  ]
    .filter(Boolean)
    .join(", ");
  return (
    <div className="flex flex-col gap-3 rounded-xl border border-border p-4">
      <p className="text-sm text-muted-foreground">
        Edit values in place, add lines for new variables, or delete a line to remove the variable.
        {hidden ? (
          <>
            {" "}
            {hidden} value{hidden === 1 ? "" : "s"} couldn't be shown: leave <span className="font-mono">NAME=</span> empty to keep {hidden === 1 ? "it" : "them"}.
          </>
        ) : null}
        {sealed ? ` ${sealed} sealed variable${sealed === 1 ? " isn't" : "s aren't"} listed here; edit ${sealed === 1 ? "it" : "them"} from the list.` : ""}
      </p>
      {values === null ? (
        <Skeleton className="h-48" />
      ) : (
        <Textarea aria-label="Raw variables" className="min-h-48 font-mono" spellCheck={false} value={text} onChange={(e) => setText(e.target.value)} />
      )}
      {parsed.errors.length ? <Alert tone="warn">{parsed.errors.slice(0, 5).join(" · ")}</Alert> : null}
      {diff.delete.length ? (
        <Alert tone="warn">
          Removes {diff.delete.slice(0, 6).join(", ")}
          {diff.delete.length > 6 ? ` and ${diff.delete.length - 6} more` : ""}.
        </Alert>
      ) : null}
      <div className="flex items-center justify-end gap-2">
        <span className="mr-auto text-[13px] text-muted-foreground">{summary}</span>
        <Button size="sm" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <Button
          size="sm"
          variant="secondary"
          loading={busy}
          disabled={total === 0 || parsed.errors.length > 0}
          onClick={() =>
            void onSubmit({ set: Object.keys(diff.set).length ? diff.set : undefined, delete: diff.delete.length ? diff.delete : undefined })
          }
        >
          {total ? `Apply ${total} change${total === 1 ? "" : "s"}` : "No changes"}
        </Button>
      </div>
    </div>
  );
}
