import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Plus, Trash } from "@/components/ui/icons";
import { Field, Input, Select } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import { COLUMN_TYPES, addColumnSql, createTableSql, type ColumnDraft } from "@/lib/sql";

const blank = (): ColumnDraft => ({ name: "", type: "text", nullable: true, defaultExpr: "" });

function ColumnRow({
  c,
  onChange,
  onRemove,
  pk,
}: {
  c: ColumnDraft;
  onChange: (c: ColumnDraft) => void;
  onRemove?: () => void;
  pk?: boolean;
}) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <Input className="w-36" aria-label="Column name" placeholder="name" value={c.name} onChange={(e) => onChange({ ...c, name: e.target.value })} />
      <Select className="w-40" aria-label="Type" value={c.type} onChange={(e) => onChange({ ...c, type: e.target.value })}>
        {COLUMN_TYPES.map((t) => (
          <option key={t}>{t}</option>
        ))}
      </Select>
      <Input
        mono
        className="w-36"
        aria-label="Default"
        placeholder="default"
        value={c.defaultExpr ?? ""}
        onChange={(e) => onChange({ ...c, defaultExpr: e.target.value })}
      />
      {pk ? (
        <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <input type="checkbox" className="size-3.5 accent-foreground" checked={!!c.primaryKey} onChange={(e) => onChange({ ...c, primaryKey: e.target.checked })} />
          PK
        </label>
      ) : null}
      <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
        <input
          type="checkbox"
          className="size-3.5 accent-foreground"
          checked={c.nullable && !c.primaryKey}
          disabled={c.primaryKey}
          onChange={(e) => onChange({ ...c, nullable: e.target.checked })}
        />
        Nullable
      </label>
      {onRemove ? (
        <button type="button" aria-label="Remove column" onClick={onRemove} className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-error">
          <Trash className="size-4" />
        </button>
      ) : null}
    </div>
  );
}

/** Generates CREATE TABLE and hands the SQL to `onRun`. */
export function NewTableDialog({
  open,
  onOpenChange,
  running,
  error,
  onRun,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  running: boolean;
  error: string | null;
  onRun: (sql: string) => void;
}) {
  const [name, setName] = useState("");
  const [cols, setCols] = useState<ColumnDraft[]>([
    { name: "id", type: "bigserial", nullable: false, primaryKey: true },
    { name: "created_at", type: "timestamptz", nullable: false, defaultExpr: "now()" },
  ]);
  const valid = name.trim() && cols.length > 0 && cols.every((c) => c.name.trim());
  const [localError, setLocalError] = useState<string | null>(null);
  const submit = () => {
    try {
      onRun(createTableSql(name.trim(), cols.map((c) => ({ ...c, name: c.name.trim() }))));
      setLocalError(null);
    } catch (e) {
      setLocalError(e instanceof Error ? e.message : "Invalid table");
    }
  };
  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      width={640}
      title="New table"
      footer={
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button variant="secondary" loading={running} disabled={!valid} onClick={submit}>
            Create table
          </Button>
        </>
      }
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) submit();
        }}
      >
        {error || localError ? <Alert>{localError ?? error}</Alert> : null}
        <Field label="Name">{(p) => <Input {...p} autoFocus autoComplete="off" value={name} onChange={(e) => setName(e.target.value)} />}</Field>
        <div className="flex flex-col gap-2">
          <div className="text-sm font-medium">Columns</div>
          {cols.map((c, i) => (
            <ColumnRow
              key={i}
              c={c}
              pk
              onChange={(n) => setCols((l) => l.map((x, j) => (j === i ? n : x)))}
              onRemove={cols.length > 1 ? () => setCols((l) => l.filter((_, j) => j !== i)) : undefined}
            />
          ))}
          <div>
            <Button type="button" size="sm" variant="ghost" icon={<Plus />} onClick={() => setCols((l) => [...l, blank()])}>
              Add column
            </Button>
          </div>
        </div>
      </form>
    </Modal>
  );
}

export function AddColumnDialog({
  open,
  onOpenChange,
  table,
  running,
  error,
  onRun,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  table: string;
  running: boolean;
  error: string | null;
  onRun: (sql: string) => void;
}) {
  const [c, setC] = useState<ColumnDraft>(blank());
  const [localError, setLocalError] = useState<string | null>(null);
  const valid = !!c.name.trim();
  const submit = () => {
    try {
      onRun(addColumnSql(table, { ...c, name: c.name.trim() }));
      setLocalError(null);
    } catch (e) {
      setLocalError(e instanceof Error ? e.message : "Invalid column");
    }
  };
  return (
    <Modal
      open={open}
      onOpenChange={onOpenChange}
      width={600}
      title="Add column"
      description={`Adds a column to ${table}.`}
      footer={
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button variant="secondary" loading={running} disabled={!valid} onClick={submit}>
            Add column
          </Button>
        </>
      }
    >
      <form
        className="flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (valid) submit();
        }}
      >
        {error || localError ? <Alert>{localError ?? error}</Alert> : null}
        <ColumnRow c={c} onChange={setC} />
        <p className="text-xs text-muted-foreground">A NOT NULL column on a table with rows needs a default.</p>
      </form>
    </Modal>
  );
}
