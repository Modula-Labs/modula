import * as Dialog from "@radix-ui/react-dialog";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/Button";
import { X } from "@/components/ui/icons";
import { Textarea, Input } from "@/components/ui/Input";
import { Modal } from "@/components/ui/Modal";
import { Alert } from "@/components/ui/misc";
import type { DbColumn } from "@/lib/api";

interface FieldState {
  value: string;
  isNull: boolean;
}

/** `row` undefined = inserting a new row. */
export function RowDrawer({
  columns,
  row,
  canEdit,
  saving,
  deleting,
  error,
  onClose,
  onSave,
  onDelete,
}: {
  columns: DbColumn[];
  row?: (string | null)[];
  canEdit: boolean;
  saving: boolean;
  deleting: boolean;
  error: string | null;
  onClose: () => void;
  /** Only the changed (or, for inserts, filled) fields. */
  onSave: (values: Record<string, string | null>) => void;
  onDelete?: () => void;
}) {
  const inserting = !row;
  const pk = columns.filter((c) => c.primaryKey);
  const editable = canEdit && (inserting || pk.length > 0);
  const initial = (): Record<string, FieldState> =>
    Object.fromEntries(columns.map((c, i) => [c.name, { value: row?.[i] ?? "", isNull: row ? row[i] === null : false }]));
  const [fields, setFields] = useState(initial);
  const [confirm, setConfirm] = useState(false);
  useEffect(() => setFields(initial()), [row, columns]); // eslint-disable-line react-hooks/exhaustive-deps

  const set = (name: string, f: Partial<FieldState>) => setFields((s) => ({ ...s, [name]: { ...s[name]!, ...f } }));

  const changes = (): Record<string, string | null> => {
    const out: Record<string, string | null> = {};
    columns.forEach((c, i) => {
      const f = fields[c.name]!;
      const next = f.isNull ? null : f.value;
      if (inserting) {
        if (f.isNull || f.value !== "") out[c.name] = next;
      } else if (next !== row![i]) out[c.name] = next;
    });
    return out;
  };
  const dirty = Object.keys(changes()).length > 0;

  return (
    <Dialog.Root open onOpenChange={(o) => !o && onClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/40" />
        <Dialog.Content className="anim-sheet fixed top-3 right-3 bottom-3 z-50 flex w-[min(460px,calc(100vw-24px))] flex-col rounded-xl border border-border bg-card shadow-modal">
          <div className="flex items-center justify-between gap-3 px-5 pt-5 pb-3">
            <div>
              <Dialog.Title className="text-lg font-semibold">{inserting ? "New row" : "Row"}</Dialog.Title>
              <Dialog.Description className="text-sm text-muted-foreground">
                {inserting
                  ? "Leave a field empty to use its default."
                  : pk.length === 0
                    ? "This table has no primary key, so rows are read-only."
                    : `Identified by ${pk.map((c) => c.name).join(", ")}.`}
              </Dialog.Description>
            </div>
            <Dialog.Close aria-label="Close" className="rounded-md p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground">
              <X className="size-4" />
            </Dialog.Close>
          </div>
          <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-auto px-5 py-3">
            {error ? <Alert>{error}</Alert> : null}
            {columns.map((c) => {
              const f = fields[c.name]!;
              const long = f.value.includes("\n") || f.value.length > 80 || /json|text/.test(c.type);
              const id = `row-${c.name}`;
              return (
                <div key={c.name} className="flex flex-col gap-1.5">
                  <div className="flex items-center justify-between gap-2">
                    <label htmlFor={id} className="text-sm font-medium">
                      {c.name}
                      {c.primaryKey ? <span className="ml-1.5 text-xs font-normal text-muted-foreground">primary key</span> : null}
                    </label>
                    <span className="font-mono text-xs text-muted-foreground">{c.type}</span>
                  </div>
                  {long ? (
                    <Textarea
                      id={id}
                      rows={3}
                      disabled={!editable || f.isNull}
                      value={f.isNull ? "" : f.value}
                      placeholder={f.isNull ? "NULL" : undefined}
                      onChange={(e) => set(c.name, { value: e.target.value })}
                    />
                  ) : (
                    <Input
                      id={id}
                      mono
                      disabled={!editable || f.isNull}
                      value={f.isNull ? "" : f.value}
                      placeholder={f.isNull ? "NULL" : undefined}
                      onChange={(e) => set(c.name, { value: e.target.value })}
                    />
                  )}
                  {c.nullable && editable ? (
                    <label className="flex items-center gap-2 text-xs text-muted-foreground">
                      <input
                        type="checkbox"
                        className="size-3.5 accent-foreground"
                        checked={f.isNull}
                        onChange={(e) => set(c.name, { isNull: e.target.checked })}
                      />
                      NULL
                    </label>
                  ) : null}
                </div>
              );
            })}
          </div>
          <div className="flex items-center justify-between gap-2 border-t border-border px-5 py-3">
            {!inserting && editable ? (
              <Button variant="danger-outline" onClick={() => setConfirm(true)}>
                Delete row
              </Button>
            ) : (
              <span />
            )}
            <div className="flex gap-2">
              <Button variant="ghost" onClick={onClose}>
                {editable ? "Cancel" : "Close"}
              </Button>
              {editable ? (
                <Button variant="secondary" loading={saving} disabled={!dirty} onClick={() => onSave(changes())}>
                  {inserting ? "Insert" : "Save"}
                </Button>
              ) : null}
            </div>
          </div>
          <Modal
            open={confirm}
            onOpenChange={setConfirm}
            title="Delete row"
            description="This permanently deletes the row. It can't be undone."
            footer={
              <>
                <Button variant="ghost" onClick={() => setConfirm(false)}>
                  Cancel
                </Button>
                <Button
                  variant="danger"
                  loading={deleting}
                  onClick={() => {
                    onDelete?.();
                    setConfirm(false);
                  }}
                >
                  Delete row
                </Button>
              </>
            }
          />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
