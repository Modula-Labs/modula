import { clsx } from "clsx";
import { SortAsc, SortDesc } from "@/components/ui/icons";

export interface GridColumn {
  name: string;
  type?: string;
  primaryKey?: boolean;
}

/** Sticky-header grid: ~50px rows, horizontal scroll, optional sort + row click. */
export function DataGrid({
  columns,
  rows,
  sort,
  onSort,
  onRowClick,
  dim,
}: {
  columns: GridColumn[];
  rows: (string | null)[][];
  sort?: { column: string; dir: "asc" | "desc" } | null;
  onSort?: (column: string) => void;
  onRowClick?: (row: (string | null)[], index: number) => void;
  /** Fades the grid while a new page loads. */
  dim?: boolean;
}) {
  return (
    <div className={clsx("max-h-[560px] overflow-auto rounded-lg border border-border transition-opacity", dim && "opacity-60")}>
      <table className="w-max min-w-full border-collapse text-left">
        <thead className="sticky top-0 z-10 bg-card">
          <tr className="border-b border-border">
            {columns.map((c) => {
              const active = sort?.column === c.name;
              const head = (
                <>
                  <span>{c.name}</span>
                  {c.type ? <span className="font-normal opacity-70">{c.type}</span> : null}
                  {active ? sort.dir === "asc" ? <SortAsc className="size-3" /> : <SortDesc className="size-3" /> : null}
                </>
              );
              return (
                <th key={c.name} className="h-9 px-3 text-xs font-medium whitespace-nowrap text-muted-foreground">
                  {onSort ? (
                    <button type="button" onClick={() => onSort(c.name)} className="inline-flex items-center gap-1.5 hover:text-foreground">
                      {head}
                    </button>
                  ) : (
                    <span className="inline-flex items-center gap-1.5">{head}</span>
                  )}
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, ri) => (
            <tr
              key={ri}
              onClick={onRowClick ? () => onRowClick(row, ri) : undefined}
              onKeyDown={
                onRowClick
                  ? (e) => {
                      if (e.key === "Enter") onRowClick(row, ri);
                    }
                  : undefined
              }
              tabIndex={onRowClick ? 0 : undefined}
              className={clsx("border-b border-border last:border-b-0 hover:bg-accent/60", onRowClick && "cursor-pointer")}
            >
              {row.map((v, ci) => (
                <td key={ci} className="h-[50px] max-w-[320px] px-3 font-mono text-[13px] whitespace-nowrap">
                  {v === null ? (
                    <span className="text-muted-foreground italic">NULL</span>
                  ) : (
                    <span className="block truncate" title={v.length > 40 ? v : undefined}>
                      {v === "" ? <span className="text-muted-foreground italic">empty</span> : v}
                    </span>
                  )}
                </td>
              ))}
            </tr>
          ))}
          {rows.length === 0 ? (
            <tr>
              <td colSpan={Math.max(columns.length, 1)} className="h-24 px-3 text-center text-sm text-muted-foreground">
                No rows.
              </td>
            </tr>
          ) : null}
        </tbody>
      </table>
    </div>
  );
}
