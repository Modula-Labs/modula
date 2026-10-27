import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { Button } from "@/components/ui/Button";
import { ChevronLeft, ChevronRight, ChevronsLeft, ChevronsRight, Plus, Refresh, Search, Table } from "@/components/ui/icons";
import { Input } from "@/components/ui/Input";
import { Alert, Badge, Skeleton } from "@/components/ui/misc";
import { api, type DbQueryResult, type DbRows, type Instance } from "@/lib/api";
import { formatCount, formatMs } from "@/lib/format";
import { samplePlaceholder, tableRef } from "@/lib/sql";
import { DataGrid } from "./DataGrid";
import { RowDrawer } from "./RowDrawer";
import { AddColumnDialog, NewTableDialog } from "./SchemaDialogs";
import { SqlBox } from "./SqlBox";
import { IconButton, QueryBoundary, dbQueryOpts } from "./shared";

const PAGE_SIZE = 50;
const msg = (e: unknown) => (e instanceof Error ? e.message : "Something went wrong.");

export function DataView({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const qc = useQueryClient();
  const id = instance.id;
  const [table, setTable] = useState<string | null>(null);
  const [result, setResult] = useState<DbQueryResult | null>(null);
  const [newTable, setNewTable] = useState(false);
  const [addColumn, setAddColumn] = useState(false);

  const tables = useQuery({ queryKey: ["db-tables", id], queryFn: () => api.dbTables(id), ...dbQueryOpts });
  const refs = useMemo(() => (tables.data?.tables ?? []).map(tableRef), [tables.data]);

  const run = useMutation({
    mutationFn: (sql: string) => api.dbQuery(id, sql),
    onSuccess: (r) => {
      setResult(r);
      // Anything that isn't a plain read may have changed the schema or the data.
      if (r.command.toUpperCase() !== "SELECT") {
        void qc.invalidateQueries({ queryKey: ["db-tables", id] });
        void qc.invalidateQueries({ queryKey: ["db-rows", id] });
        void qc.invalidateQueries({ queryKey: ["db-stats", id] });
      }
    },
  });
  // Schema dialogs run through the same endpoint but keep their own error state.
  const schema = useMutation({
    mutationFn: (sql: string) => api.dbQuery(id, sql),
    onSuccess: () => {
      setNewTable(false);
      setAddColumn(false);
      void qc.invalidateQueries({ queryKey: ["db-tables", id] });
      void qc.invalidateQueries({ queryKey: ["db-rows", id] });
      void qc.invalidateQueries({ queryKey: ["db-stats", id] });
    },
  });

  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["db-tables", id] });
    void qc.invalidateQueries({ queryKey: ["db-rows", id] });
  };
  const refreshing = tables.isFetching || qc.isFetching({ queryKey: ["db-rows", id] }) > 0;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between gap-3">
        {table ? (
          <nav className="flex min-w-0 items-center gap-1.5 text-base font-bold">
            <button
              type="button"
              className="cursor-pointer text-muted-foreground hover:text-foreground"
              onClick={() => {
                setTable(null);
                setResult(null);
              }}
            >
              Tables
            </button>
            <span className="text-muted-foreground">/</span>
            <span className="truncate">{table}</span>
          </nav>
        ) : (
          <h3 className="text-base font-bold">Tables</h3>
        )}
        <div className="flex items-center gap-2">
          <IconButton label="Refresh" disabled={refreshing} onClick={refresh}>
            <Refresh className={refreshing ? "animate-spin" : undefined} />
          </IconButton>
          {canEdit ? (
            table ? (
              <Button variant="outline" icon={<Plus />} onClick={() => setAddColumn(true)}>
                Column
              </Button>
            ) : (
              <Button variant="outline" icon={<Plus />} onClick={() => setNewTable(true)}>
                New table
              </Button>
            )
          ) : null}
        </div>
      </div>

      {canEdit ? (
        <SqlBox placeholder={samplePlaceholder(table ? [table] : refs)} running={run.isPending} onRun={(sql) => run.mutate(sql)} />
      ) : null}
      {run.error ? <Alert>{msg(run.error)}</Alert> : null}

      {result ? (
        <ResultPanel result={result} onClear={() => setResult(null)} />
      ) : table ? (
        <TableView key={table} instance={instance} table={table} canEdit={canEdit} />
      ) : (
        <QueryBoundary q={tables} skeleton={<TilesSkeleton />}>
          {(d) => <TileGrid tables={d.tables} onOpen={(t) => setTable(tableRef(t))} />}
        </QueryBoundary>
      )}

      <NewTableDialog
        key={String(newTable)}
        open={newTable}
        onOpenChange={(o) => {
          setNewTable(o);
          schema.reset();
        }}
        running={schema.isPending}
        error={schema.error ? msg(schema.error) : null}
        onRun={(sql) => schema.mutate(sql)}
      />
      {table ? (
        <AddColumnDialog
          key={String(addColumn)}
          open={addColumn}
          onOpenChange={(o) => {
            setAddColumn(o);
            schema.reset();
          }}
          table={table}
          running={schema.isPending}
          error={schema.error ? msg(schema.error) : null}
          onRun={(sql) => schema.mutate(sql)}
        />
      ) : null}
    </div>
  );
}

function TilesSkeleton() {
  return (
    <div className="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-3">
      {Array.from({ length: 6 }, (_, i) => (
        <Skeleton key={i} className="h-14 rounded-xl" />
      ))}
    </div>
  );
}

function TileGrid({ tables, onOpen }: { tables: { schema: string; name: string; rowEstimate: number }[]; onOpen: (t: { schema: string; name: string }) => void }) {
  const [searching, setSearching] = useState(false);
  const [q, setQ] = useState("");
  const list = tables.filter((t) => t.name.toLowerCase().includes(q.toLowerCase()));
  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center gap-2">
        <IconButton label="Search tables" onClick={() => setSearching((s) => !s)} className={searching ? "bg-accent text-foreground" : undefined}>
          <Search />
        </IconButton>
        {searching ? <Input autoFocus className="h-8 max-w-xs" placeholder="Search tables…" value={q} onChange={(e) => setQ(e.target.value)} /> : null}
        <span className="text-[13px] text-muted-foreground">{formatCount(tables.length)} tables</span>
      </div>
      {tables.length === 0 ? (
        <p className="rounded-xl border border-dashed border-border px-6 py-10 text-center text-sm text-muted-foreground">
          No tables yet. Create one with New table or run a CREATE TABLE statement.
        </p>
      ) : (
        <div className="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-3">
          {list.map((t) => (
            <button
              key={`${t.schema}.${t.name}`}
              type="button"
              onClick={() => onOpen(t)}
              title={`${t.schema}.${t.name} · ~${formatCount(t.rowEstimate)} rows`}
              className="flex h-14 cursor-pointer items-center gap-2.5 rounded-xl border border-border bg-card px-3 text-left text-sm outline-none hover:bg-muted/50 focus-visible:ring-3 focus-visible:ring-ring/50"
            >
              <Table className="size-4 shrink-0 text-muted-foreground" />
              <span className="truncate">{t.schema === "public" ? t.name : `${t.schema}.${t.name}`}</span>
            </button>
          ))}
          {list.length === 0 ? <p className="col-span-full text-sm text-muted-foreground">No tables match.</p> : null}
        </div>
      )}
    </div>
  );
}

function ResultPanel({ result, onClear }: { result: DbQueryResult; onClear: () => void }) {
  const hasGrid = result.columns.length > 0;
  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between gap-2 text-sm">
        <div className="flex flex-wrap items-center gap-2 text-muted-foreground">
          <Badge>{result.command}</Badge>
          <span>
            {formatCount(result.rowCount)} {result.rowCount === 1 ? "row" : "rows"} · {formatMs(result.durationMs)}
          </span>
          {result.truncated ? <Badge tone="warn">Truncated</Badge> : null}
        </div>
        <Button variant="ghost" onClick={onClear}>
          Clear results
        </Button>
      </div>
      {hasGrid ? <DataGrid columns={result.columns} rows={result.rows} /> : <p className="text-sm text-muted-foreground">Statement ran successfully.</p>}
    </div>
  );
}

function TableView({ instance, table, canEdit }: { instance: Instance; table: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const id = instance.id;
  const [page, setPage] = useState(1);
  const [sort, setSort] = useState<{ column: string; dir: "asc" | "desc" } | null>(null);
  const [selected, setSelected] = useState<number | "new" | null>(null);

  const q = useQuery({
    queryKey: ["db-rows", id, table, page, sort?.column ?? null, sort?.dir ?? null],
    queryFn: () => api.dbRows(id, { table, page, pageSize: PAGE_SIZE, orderBy: sort?.column, dir: sort?.dir }),
    placeholderData: keepPreviousData,
    ...dbQueryOpts,
  });
  const data = q.data;
  const pages = data ? Math.max(1, Math.ceil(data.total / data.pageSize)) : 1;

  const after = () => {
    setSelected(null);
    void qc.invalidateQueries({ queryKey: ["db-rows", id, table] });
    void qc.invalidateQueries({ queryKey: ["db-tables", id] });
    void qc.invalidateQueries({ queryKey: ["db-stats", id] });
  };
  const keyOf = (d: DbRows, row: (string | null)[]) =>
    Object.fromEntries(d.columns.flatMap((c, i) => (c.primaryKey ? [[c.name, row[i] ?? null] as const] : [])));
  const save = useMutation({
    mutationFn: (values: Record<string, string | null>) =>
      selected === "new" || selected === null || !data
        ? api.dbInsertRow(id, table, values)
        : api.dbUpdateRow(id, table, keyOf(data, data.rows[selected]!), values),
    onSuccess: after,
  });
  const remove = useMutation({
    mutationFn: () => api.dbDeleteRow(id, table, keyOf(data!, data!.rows[selected as number]!)),
    onSuccess: after,
  });

  const onSort = (column: string) => {
    setPage(1);
    setSort((s) => (s?.column !== column ? { column, dir: "asc" } : s.dir === "asc" ? { column, dir: "desc" } : null));
  };

  return (
    <QueryBoundary q={q} skeleton={<Skeleton className="h-64" />}>
      {(d) => (
        <div className="flex flex-col gap-3">
          <DataGrid
            columns={d.columns}
            rows={d.rows}
            sort={sort}
            onSort={onSort}
            dim={q.isPlaceholderData}
            onRowClick={(_, i) => {
              save.reset();
              remove.reset();
              setSelected(i);
            }}
          />
          <div className="flex items-center justify-between">
            {canEdit ? (
              <Button
                variant="outline"
                icon={<Plus />}
                onClick={() => {
                  save.reset();
                  setSelected("new");
                }}
              >
                Row
              </Button>
            ) : (
              <span />
            )}
            <span className="text-[13px] text-muted-foreground">{formatCount(d.total)} rows</span>
          </div>
          <div className="flex items-center justify-center gap-1 text-sm">
            <IconButton label="First page" disabled={page <= 1} onClick={() => setPage(1)}>
              <ChevronsLeft />
            </IconButton>
            <IconButton label="Previous page" disabled={page <= 1} onClick={() => setPage((p) => p - 1)}>
              <ChevronLeft />
            </IconButton>
            <span className="px-2 text-muted-foreground">
              Page {page} of {formatCount(pages)}
            </span>
            <IconButton label="Next page" disabled={page >= pages} onClick={() => setPage((p) => p + 1)}>
              <ChevronRight />
            </IconButton>
            <IconButton label="Last page" disabled={page >= pages} onClick={() => setPage(pages)}>
              <ChevronsRight />
            </IconButton>
          </div>
          {selected !== null ? (
            <RowDrawer
              columns={d.columns}
              row={selected === "new" ? undefined : d.rows[selected]}
              canEdit={canEdit}
              saving={save.isPending}
              deleting={remove.isPending}
              error={save.error ? msg(save.error) : remove.error ? msg(remove.error) : null}
              onClose={() => setSelected(null)}
              onSave={(v) => save.mutate(v)}
              onDelete={() => remove.mutate()}
            />
          ) : null}
        </div>
      )}
    </QueryBoundary>
  );
}
