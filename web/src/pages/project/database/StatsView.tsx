import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/Button";
import { Refresh } from "@/components/ui/icons";
import { Alert, Badge } from "@/components/ui/misc";
import { api, type DbStats, type Instance } from "@/lib/api";
import { formatBytes, formatCount, formatMs, formatRatio, relativeTime } from "@/lib/format";
import { Card, IconButton, QueryBoundary, SectionTitle, dbQueryOpts } from "./shared";

/** Postgres wraps transaction ids at 2^31; autovacuum starts freezing at 200M. */
const XID_LIMIT = 2_147_483_648;
const BLOAT_PCT = 10;

function StatCard({ label, value, sub, pct }: { label: string; value: ReactNode; sub?: ReactNode; pct?: number }) {
  return (
    <Card className="flex flex-col gap-1 p-4">
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="text-2xl font-semibold tabular-nums">{value}</div>
      {sub ? <div className="text-xs text-muted-foreground tabular-nums">{sub}</div> : null}
      {pct !== undefined ? (
        <div className="mt-1 h-1 overflow-hidden rounded-full bg-muted">
          <div className="h-full rounded-full bg-foreground/70" style={{ width: `${Math.min(100, Math.max(0, pct))}%` }} />
        </div>
      ) : null}
    </Card>
  );
}

function Th({ children, right }: { children: ReactNode; right?: boolean }) {
  return <th className={`h-9 px-3 text-xs font-medium whitespace-nowrap text-muted-foreground ${right ? "text-right" : "text-left"}`}>{children}</th>;
}
function Td({ children, right, mono }: { children: ReactNode; right?: boolean; mono?: boolean }) {
  return <td className={`h-10 px-3 text-sm whitespace-nowrap tabular-nums ${right ? "text-right" : ""} ${mono ? "font-mono text-[13px]" : ""}`}>{children}</td>;
}
const TableCard = ({ children }: { children: ReactNode }) => (
  <Card className="overflow-x-auto">
    <table className="w-full border-collapse">{children}</table>
  </Card>
);
const rowCls = "border-t border-border first:border-t-0";

export function StatsView({ instance, canEdit }: { instance: Instance; canEdit: boolean }) {
  const q = useQuery({ queryKey: ["db-stats", instance.id], queryFn: () => api.dbStats(instance.id), refetchInterval: 15_000, ...dbQueryOpts });
  return (
    <QueryBoundary q={q}>
      {(s) => (
        <div className="flex flex-col gap-8">
          <div className="flex items-center justify-end">
            <IconButton label="Refresh" disabled={q.isFetching} onClick={() => void q.refetch()}>
              <Refresh className={q.isFetching ? "animate-spin" : undefined} />
            </IconButton>
          </div>
          <Connections s={s} />
          <QueryStatistics instance={instance} s={s} canEdit={canEdit} />
          <TablesSection s={s} />
          <VacuumHealth instance={instance} s={s} canEdit={canEdit} />
          <IndexHealth s={s} />
        </div>
      )}
    </QueryBoundary>
  );
}

function Connections({ s }: { s: DbStats }) {
  const c = s.connections;
  const pct = c.max > 0 ? (c.used / c.max) * 100 : 0;
  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-5">
      <StatCard label="Connections" value={`${Math.round(pct)}%`} sub={`${formatCount(c.used)} / ${formatCount(c.max)}`} pct={pct} />
      <StatCard label="Active" value={formatCount(c.active)} />
      <StatCard label="Idle" value={formatCount(c.idle)} />
      <StatCard label="Idle in Txn" value={formatCount(c.idleInTransaction)} />
      <StatCard label="Cache Hit" value={formatRatio(s.cacheHitRatio)} />
    </div>
  );
}

function QueryStatistics({ instance, s, canEdit }: { instance: Instance; s: DbStats; canEdit: boolean }) {
  const qc = useQueryClient();
  const enable = useMutation({
    mutationFn: () => api.dbSetExtension(instance.id, "pg_stat_statements", "install"),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["db-stats", instance.id] });
      void qc.invalidateQueries({ queryKey: ["db-extensions", instance.id] });
    },
  });
  return (
    <section>
      <SectionTitle title="Query Statistics" />
      {s.pgStatStatements ? (
        s.topQueries.length === 0 ? (
          <p className="text-sm text-muted-foreground">No queries recorded yet.</p>
        ) : (
          <TableCard>
            <thead>
              <tr>
                <Th>Query</Th>
                <Th right>Calls</Th>
                <Th right>Mean</Th>
                <Th right>Total</Th>
              </tr>
            </thead>
            <tbody>
              {s.topQueries.map((r, i) => (
                <tr key={i} className={rowCls}>
                  <Td mono>
                    <span className="block max-w-[420px] truncate" title={r.query}>
                      {r.query}
                    </span>
                  </Td>
                  <Td right>{formatCount(r.calls)}</Td>
                  <Td right>{formatMs(r.meanMs)}</Td>
                  <Td right>{formatMs(r.totalMs)}</Td>
                </tr>
              ))}
            </tbody>
          </TableCard>
        )
      ) : (
        <Card className="flex items-center justify-between gap-4 p-4">
          <p className="text-sm text-muted-foreground">Install the pg_stat_statements extension to see your slowest and most frequent queries.</p>
          {canEdit ? (
            <Button variant="secondary" size="sm" loading={enable.isPending} onClick={() => enable.mutate()}>
              Enable
            </Button>
          ) : null}
        </Card>
      )}
      {enable.error ? (
        <div className="mt-2">
          <Alert>{enable.error.message}</Alert>
        </div>
      ) : null}
    </section>
  );
}

function TablesSection({ s }: { s: DbStats }) {
  const [all, setAll] = useState(false);
  const sorted = [...s.tables].sort((a, b) => b.dataBytes + b.indexBytes - (a.dataBytes + a.indexBytes));
  const shown = all ? sorted : sorted.slice(0, 10);
  const z = s.sizes;
  return (
    <section>
      <SectionTitle title="Tables" />
      <div className="mb-4 grid grid-cols-2 gap-3 md:grid-cols-6">
        <StatCard label="Database" value={formatBytes(z.database)} />
        <StatCard label="Tables" value={formatBytes(z.tables)} />
        <StatCard label="Indexes" value={formatBytes(z.indexes)} />
        <StatCard label="System" value={formatBytes(z.system)} />
        <StatCard label="Other" value={formatBytes(z.other)} />
        <StatCard label="WAL" value={formatBytes(z.wal)} />
      </div>
      <TableCard>
        <thead>
          <tr>
            <Th>Table</Th>
            <Th right>Rows</Th>
            <Th right>Data</Th>
            <Th right>Indexes</Th>
            <Th right>Seq Scans</Th>
            <Th right>Idx Scans</Th>
          </tr>
        </thead>
        <tbody>
          {shown.map((t) => (
            <tr key={t.name} className={rowCls}>
              <Td mono>{t.name}</Td>
              <Td right>{formatCount(t.rows)}</Td>
              <Td right>{formatBytes(t.dataBytes)}</Td>
              <Td right>{formatBytes(t.indexBytes)}</Td>
              <Td right>{formatCount(t.seqScans)}</Td>
              <Td right>{formatCount(t.idxScans)}</Td>
            </tr>
          ))}
          {sorted.length === 0 ? (
            <tr>
              <td colSpan={6} className="h-16 text-center text-sm text-muted-foreground">
                No tables.
              </td>
            </tr>
          ) : null}
        </tbody>
      </TableCard>
      {sorted.length > 10 ? (
        <div className="mt-2">
          <Button variant="ghost" size="sm" onClick={() => setAll((a) => !a)}>
            {all ? "Show fewer" : `Show all ${formatCount(sorted.length)} tables`}
          </Button>
        </div>
      ) : null}
    </section>
  );
}

function VacuumHealth({ instance, s, canEdit }: { instance: Instance; s: DbStats; canEdit: boolean }) {
  const qc = useQueryClient();
  const [pending, setPending] = useState<string | null>(null);
  const vacuum = useMutation({
    mutationFn: (table: string) => {
      setPending(table);
      return api.dbVacuum(instance.id, table);
    },
    onSettled: () => {
      setPending(null);
      void qc.invalidateQueries({ queryKey: ["db-stats", instance.id] });
    },
  });
  const [all, setAll] = useState(false);
  const dead = s.tables.reduce((n, t) => n + t.deadRows, 0);
  const bloated = s.tables.filter((t) => t.deadPct >= BLOAT_PCT).length;
  const maxAge = s.tables.reduce((n, t) => Math.max(n, t.xidAge), 0);
  const agePct = (maxAge / XID_LIMIT) * 100;
  const risk = agePct >= 75 ? "High" : agePct >= 50 ? "Elevated" : "Low";
  const sorted = [...s.tables].sort((a, b) => b.deadRows - a.deadRows);
  const shown = all ? sorted : sorted.slice(0, 5);
  return (
    <section>
      <SectionTitle title="Vacuum Health" />
      <div className="mb-4 grid grid-cols-1 gap-3 md:grid-cols-3">
        <StatCard label="Total Dead Rows" value={formatCount(dead)} />
        <StatCard label="Tables w/ Bloat" value={formatCount(bloated)} sub={`dead rows ≥ ${BLOAT_PCT}%`} />
        <StatCard label="XID Freeze Risk" value={risk} sub={`oldest xid age ${formatCount(maxAge)} (${agePct.toFixed(1)}% of limit)`} pct={agePct} />
      </div>
      {vacuum.error ? (
        <div className="mb-2">
          <Alert>{vacuum.error.message}</Alert>
        </div>
      ) : null}
      <TableCard>
        <thead>
          <tr>
            <Th>Table</Th>
            <Th right>Dead Rows</Th>
            <Th right>Dead %</Th>
            <Th>Last Vacuum</Th>
            <Th right>XID Age</Th>
            <Th>{null}</Th>
          </tr>
        </thead>
        <tbody>
          {shown.map((t) => (
            <tr key={t.name} className={rowCls}>
              <Td mono>{t.name}</Td>
              <Td right>{formatCount(t.deadRows)}</Td>
              <Td right>{t.deadPct >= BLOAT_PCT ? <Badge tone="warn">{t.deadPct.toFixed(1)}%</Badge> : `${t.deadPct.toFixed(1)}%`}</Td>
              <Td>{t.lastVacuum ? relativeTime(t.lastVacuum) : <span className="text-muted-foreground">Never</span>}</Td>
              <Td right>{formatCount(t.xidAge)}</Td>
              <Td right>
                {canEdit ? (
                  <Button size="sm" variant="ghost" loading={pending === t.name} disabled={pending !== null} onClick={() => vacuum.mutate(t.name)}>
                    Vacuum
                  </Button>
                ) : null}
              </Td>
            </tr>
          ))}
          {sorted.length === 0 ? (
            <tr>
              <td colSpan={6} className="h-16 text-center text-sm text-muted-foreground">
                No tables.
              </td>
            </tr>
          ) : null}
        </tbody>
      </TableCard>
      {sorted.length > 5 ? (
        <div className="mt-2">
          <Button variant="ghost" size="sm" onClick={() => setAll((a) => !a)}>
            {all ? "Show fewer" : `Show all ${formatCount(sorted.length)} tables`}
          </Button>
        </div>
      ) : null}
    </section>
  );
}

function IndexHealth({ s }: { s: DbStats }) {
  const total = s.unusedIndexes.reduce((n, i) => n + i.bytes, 0);
  return (
    <section>
      <SectionTitle
        title="Index Health"
        right={
          <span className="text-sm text-muted-foreground">
            {formatCount(s.unusedIndexes.length)} unused ({formatBytes(total)})
          </span>
        }
      />
      {s.unusedIndexes.length === 0 ? (
        <p className="text-sm text-muted-foreground">Every index has been used.</p>
      ) : (
        <TableCard>
          <thead>
            <tr>
              <Th>Index</Th>
              <Th>Table</Th>
              <Th right>Size</Th>
              <Th right>Scans</Th>
            </tr>
          </thead>
          <tbody>
            {s.unusedIndexes.map((i) => (
              <tr key={`${i.table}.${i.name}`} className={rowCls}>
                <Td mono>{i.name}</Td>
                <Td mono>{i.table}</Td>
                <Td right>{formatBytes(i.bytes)}</Td>
                <Td right>{formatCount(i.scans)}</Td>
              </tr>
            ))}
          </tbody>
        </TableCard>
      )}
    </section>
  );
}
