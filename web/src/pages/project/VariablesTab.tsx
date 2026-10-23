import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { CopyButton } from "@/components/ui/CopyButton";
import { DotsVertical, Eye, EyeOff, FileText, Lock, Plus, Trash, Variable } from "@/components/ui/icons";
import { Input, Textarea } from "@/components/ui/Input";
import { Menu, MenuItem, MenuSeparator } from "@/components/ui/Menu";
import { Alert, Badge, EmptyState, Skeleton } from "@/components/ui/misc";
import { api, type VariableInfo } from "@/lib/api";
import { parseDotenv } from "@/lib/dotenv";
import { relativeTime } from "@/lib/format";

/** Write-only variables: values never come back from the API except through
 *  an audited, sudo-gated reveal, and sealed ones not even then. */
export function VariablesTab({ instanceId, canEdit }: { instanceId: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const key = ["variables", instanceId];
  const vars = useQuery({ queryKey: key, queryFn: () => api.variables(instanceId) });
  const [adding, setAdding] = useState(false);
  const [raw, setRaw] = useState(false);
  const update = useMutation({
    mutationFn: (b: Parameters<typeof api.updateVariables>[1]) => api.updateVariables(instanceId, b),
    onSuccess: (list) => qc.setQueryData(key, list),
  });

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <div className="text-sm text-muted-foreground">
          {vars.data ? `${vars.data.length} service variable${vars.data.length === 1 ? "" : "s"}` : " "}
        </div>
        {canEdit ? (
          <div className="flex gap-2">
            <Button variant="ghost" size="sm" icon={<FileText />} onClick={() => setRaw((r) => !r)}>
              Raw editor
            </Button>
            <Button variant="secondary" size="sm" icon={<Plus />} onClick={() => setAdding(true)}>
              New variable
            </Button>
          </div>
        ) : null}
      </div>
      <p className="text-xs text-muted-foreground">Changes apply on the next deploy.</p>

      {update.error ? <Alert>{update.error.message}</Alert> : null}
      {raw ? <RawEditor busy={update.isPending} onSubmit={(set) => update.mutateAsync({ set }).then(() => setRaw(false))} /> : null}
      {adding ? (
        <NewVariableRow
          busy={update.isPending}
          onCancel={() => setAdding(false)}
          onSubmit={(name, value, sealed) =>
            update.mutateAsync({ set: { [name]: value }, sealed: sealed ? [name] : [] }).then(() => setAdding(false))
          }
        />
      ) : null}

      {vars.isPending ? (
        <Skeleton className="h-32" />
      ) : vars.data && vars.data.length === 0 && !adding && !raw ? (
        <EmptyState
          icon={<Variable />}
          title="No variables yet"
          body="Variables are encrypted at rest and injected into the container on deploy."
        />
      ) : (
        <div className="divide-y divide-border rounded-lg border border-border">
          {vars.data?.map((v) => (
            <VariableRow
              key={v.id}
              v={v}
              canEdit={canEdit}
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
  onSet,
  onDelete,
}: {
  v: VariableInfo;
  canEdit: boolean;
  onSet: (value: string) => Promise<unknown>;
  onDelete: () => void;
}) {
  const [revealed, setRevealed] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);

  const reveal = async () => {
    setError(null);
    try {
      setRevealed((await api.revealVariable(v.id)).value);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not reveal the value.");
    }
  };

  return (
    <div className="group flex min-h-12 items-center gap-3 px-4 py-2">
      <div className="flex w-[38%] min-w-0 items-center gap-2">
        <span className="truncate font-mono text-[13px]">{v.name}</span>
        {v.sealed ? (
          <span title="Sealed: write-only, can never be revealed">
            <Lock className="size-3.5 shrink-0 text-muted-foreground" />
          </span>
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
            <Input mono autoFocus placeholder="New value" value={value} onChange={(e) => setValue(e.target.value)} />
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
            <button aria-label="Hide value" onClick={() => setRevealed(null)} className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground">
              <EyeOff className="size-4" />
            </button>
          </>
        ) : (
          <span className="font-mono text-[13px] tracking-widest text-muted-foreground">*******</span>
        )}
        {error ? <span className="text-xs text-error">{error}</span> : null}
      </div>
      {!editing ? <span className="hidden text-xs text-muted-foreground sm:block">{relativeTime(v.updatedAt)}</span> : null}
      {v.sealed ? <Badge>Sealed</Badge> : null}
      {canEdit && !editing ? (
        <Menu
          trigger={
            <button aria-label={`Actions for ${v.name}`} className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground">
              <DotsVertical className="size-4" />
            </button>
          }
        >
          {!v.sealed ? (
            <MenuItem icon={<Eye />} onSelect={() => void reveal()}>
              Reveal value
            </MenuItem>
          ) : null}
          <MenuItem icon={<Variable />} onSelect={() => setEditing(true)}>
            Set new value
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
  onCancel,
  onSubmit,
}: {
  busy: boolean;
  onCancel: () => void;
  onSubmit: (name: string, value: string, sealed: boolean) => Promise<unknown>;
}) {
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [sealed, setSealed] = useState(false);
  return (
    <form
      className="flex flex-col gap-3 rounded-lg border border-border p-4"
      onSubmit={(e) => {
        e.preventDefault();
        void onSubmit(name.trim(), value, sealed);
      }}
    >
      <div className="grid grid-cols-[38%_1fr] gap-3">
        <Input mono autoFocus required aria-label="Variable name" placeholder="VARIABLE_NAME" value={name} onChange={(e) => setName(e.target.value.replace(/\s/g, "_"))} />
        <Input mono aria-label="Value" placeholder="value" value={value} onChange={(e) => setValue(e.target.value)} />
      </div>
      <div className="flex items-center justify-between">
        <label className="flex items-center gap-2 text-sm text-muted-foreground">
          <input type="checkbox" className="size-4 accent-foreground" checked={sealed} onChange={(e) => setSealed(e.target.checked)} />
          Seal: never revealable, even to admins
        </label>
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

function RawEditor({ busy, onSubmit }: { busy: boolean; onSubmit: (set: Record<string, string>) => Promise<unknown> }) {
  const [text, setText] = useState("");
  const parsed = parseDotenv(text);
  const count = Object.keys(parsed.vars).length;
  return (
    <div className="flex flex-col gap-3 rounded-lg border border-border p-4">
      <p className="text-sm text-muted-foreground">
        Paste <span className="font-mono">.env</span> contents. Existing values stay hidden; names you list here are added or overwritten.
      </p>
      <Textarea
        aria-label="Raw variables"
        className="min-h-40"
        spellCheck={false}
        placeholder={"DATABASE_URL=postgres://…\nAPI_KEY=…"}
        value={text}
        onChange={(e) => setText(e.target.value)}
      />
      {parsed.errors.length ? <Alert tone="warn">{parsed.errors.slice(0, 5).join(" · ")}</Alert> : null}
      <div className="flex justify-end">
        <Button size="sm" variant="secondary" loading={busy} disabled={count === 0 || parsed.errors.length > 0} onClick={() => void onSubmit(parsed.vars)}>
          {count ? `Update ${count} variable${count === 1 ? "" : "s"}` : "Update variables"}
        </Button>
      </div>
    </div>
  );
}
