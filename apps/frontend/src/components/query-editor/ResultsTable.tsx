import { memo, useEffect, useMemo, useRef, useState } from "react";
import {
  flexRender,
  getCoreRowModel,
  getPaginationRowModel,
  getSortedRowModel,
  useReactTable,
  type ColumnDef,
  type PaginationState,
  type RowData,
  type SortingFn,
  type SortingState,
} from "@tanstack/react-table";
import { ChevronDown, ChevronUp, ChevronsUpDown } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { cn, readStorage, writeStorage } from "@/lib/utils";
import { formatNumeric, isNumericType } from "@/lib/formatNumber";
import { computeDateGranularity, formatDateValue, type DateGranularity } from "@/lib/formatDate";
import type { ResultData } from "@/types/api";

declare module "@tanstack/react-table" {
  interface ColumnMeta<TData extends RowData, TValue> {
    numeric?: boolean;
  }
}

export interface ResultsTableColumnOverride {
  name: string;
  friendly_name?: string;
  visible?: boolean;
}

const NO_OVERRIDES: ResultsTableColumnOverride[] = [];
export const DEFAULT_ITEMS_PER_PAGE = 15;

// Global (not per-query) so the choice sticks across queries.
const PAGE_SIZE_STORAGE_KEY = "results-page-size";
const PAGE_SIZE_OPTIONS = [15, 25, 50, 100, 250] as const;

function readStoredPageSize(fallback: number): number {
  const raw = Number(readStorage(PAGE_SIZE_STORAGE_KEY));
  return (PAGE_SIZE_OPTIONS as readonly number[]).includes(raw) ? raw : fallback;
}

/** NUMERIC values arrive as strings (to keep precision), so the auto-detected sort is alphanumeric
 * ("1.5" < "1.25"). Compare numerically; unparseable/nullish values sort last. */
const numericSortingFn: SortingFn<Record<string, unknown>> = (rowA, rowB, columnId) => {
  const rawA = rowA.getValue(columnId);
  const rawB = rowB.getValue(columnId);
  // `Number(null)` is 0; exclude null explicitly so it sorts last, not as zero.
  const a = rawA == null ? NaN : Number(rawA);
  const b = rawB == null ? NaN : Number(rawB);
  const aValid = Number.isFinite(a);
  const bValid = Number.isFinite(b);
  if (!aValid && !bValid) return 0;
  if (!aValid) return 1;
  if (!bValid) return -1;
  return a - b;
};

function formatCell(value: unknown, friendlyType: string, dateGranularity?: DateGranularity) {
  if (value === null || value === undefined) {
    return <span className="text-muted-foreground">—</span>;
  }
  if (friendlyType === "boolean") return value ? "true" : "false";
  if (friendlyType === "date" && typeof value === "string") {
    const [y, m, d] = value.split("-").map(Number);
    if (y === undefined || m === undefined || d === undefined) return value;
    const date = new Date(y, m - 1, d);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleDateString();
  }
  if (friendlyType === "datetime" && typeof value === "string") {
    return formatDateValue(value, dateGranularity ?? "datetime");
  }
  if (isNumericType(friendlyType) && (typeof value === "number" || typeof value === "string")) {
    return formatNumeric(value);
  }
  if (typeof value === "object") return <code className="text-xs">{JSON.stringify(value)}</code>;
  return String(value);
}

interface ResultsTableProps {
  data: ResultData;
  /** Per-column display overrides, exposed as options by the "Table" visualization. */
  columnOverrides?: ResultsTableColumnOverride[];
  itemsPerPage?: number;
  /** Column-type badge next to each header; off for the "Table" visualization published to
   * dashboards. */
  showTypeBadges?: boolean;
}

export const ResultsTable = memo(function ResultsTable({
  data,
  columnOverrides = NO_OVERRIDES,
  itemsPerPage = DEFAULT_ITEMS_PER_PAGE,
  showTypeBadges = true,
}: ResultsTableProps) {
  const [sorting, setSorting] = useState<SortingState>([]);
  // Controlled so a saved itemsPerPage applies without remount. `showTypeBadges` doubles as "query
  // editor preview" (true) vs a published Table viz (false, own `itemsPerPage`);
  // a viewer's page-size preference must never override the author's.
  const [pagination, setPagination] = useState<PaginationState>(() => ({
    pageIndex: 0,
    pageSize: showTypeBadges ? readStoredPageSize(itemsPerPage) : itemsPerPage,
  }));
  useEffect(() => {
    if (showTypeBadges) return;
    setPagination({ pageIndex: 0, pageSize: itemsPerPage });
  }, [itemsPerPage, showTypeBadges]);

  // Separate from `pagination.pageIndex` so a partly typed number doesn't jump pages until
  // committed.
  const [pageJumpDraft, setPageJumpDraft] = useState("");

  const visibleCols = useMemo(
    () => data.columns.filter((col) => columnOverrides.find((o) => o.name === col.name)?.visible !== false),
    [data.columns, columnOverrides],
  );

  // Computed per column, not per cell, so a "datetime" column renders every row at one granularity.
  const dateGranularities = useMemo(
    (): Record<string, DateGranularity> =>
      Object.fromEntries(
        visibleCols
          .filter((col) => col.friendly_type === "datetime")
          .map((col) => [col.name, computeDateGranularity(data.rows.map((row) => row[col.name]))]),
      ),
    [visibleCols, data.rows],
  );

  const columns = useMemo<ColumnDef<Record<string, unknown>>[]>(
    () =>
      visibleCols.map((col, i) => {
        const ov = columnOverrides.find((o) => o.name === col.name);
        const label = ov?.friendly_name ?? col.name;
        const numeric = isNumericType(col.friendly_type);
        return {
          id: `${col.name}__${i}`,
          accessorFn: (row) => row[col.name],
          header: ({ column }) => (
            <button
              type="button"
              className={cn("flex cursor-pointer items-center gap-1 font-medium", numeric && "justify-end")}
              onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
            >
              {label}
              {showTypeBadges && (
                <>
                  {/* aria-hidden badge; without a separator a screen reader would read it as one run-on word ("revenuefloat"). */}
                  <span aria-hidden="true" className="text-xs font-normal text-muted-foreground">
                    {col.friendly_type}
                  </span>
                  <span className="sr-only">{`, ${col.friendly_type}`}</span>
                </>
              )}
              {column.getIsSorted() === "asc" ? (
                <ChevronUp className="size-3" />
              ) : column.getIsSorted() === "desc" ? (
                <ChevronDown className="size-3" />
              ) : (
                <ChevronsUpDown className="size-3 opacity-40" />
              )}
            </button>
          ),
          cell: (info) => formatCell(info.getValue(), col.friendly_type, dateGranularities[col.name]),
          meta: { numeric },
          // An explicit `sortingFn: undefined` breaks the table library's auto-detected fallback
          // for other columns; set the key only for numeric columns.
          ...(numeric ? { sortingFn: numericSortingFn } : {}),
        };
      }),
    [visibleCols, columnOverrides, showTypeBadges, dateGranularities],
  );

  const table = useReactTable({
    data: data.rows,
    columns,
    state: { sorting, pagination },
    onSortingChange: setSorting,
    onPaginationChange: setPagination,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
  });

  const rows = table.getRowModel().rows;
  const pageCount = table.getPageCount();
  const pageIndex = table.getState().pagination.pageIndex;
  const pageSize = table.getState().pagination.pageSize;
  const totalRows = data.rows.length;
  const rangeStart = totalRows === 0 ? 0 : pageIndex * pageSize + 1;
  const rangeEnd = Math.min((pageIndex + 1) * pageSize, totalRows);

  function handlePageSizeChange(value: string) {
    writeStorage(PAGE_SIZE_STORAGE_KEY, value);
    setPagination({ pageIndex: 0, pageSize: Number(value) });
  }

  // Sync the jump box when Previous/Next move the page externally.
  useEffect(() => {
    setPageJumpDraft(String(pageIndex + 1));
  }, [pageIndex]);

  function commitPageJump(raw: string) {
    const n = Number(raw);
    if (!raw.trim() || !Number.isFinite(n)) {
      setPageJumpDraft(String(pageIndex + 1)); // non-numeric: revert, page unchanged
      return;
    }
    const clamped = Math.min(Math.max(Math.trunc(n), 1), pageCount);
    table.setPageIndex(clamped - 1);
    setPageJumpDraft(String(clamped));
  }

  const scrollRef = useRef<HTMLDivElement>(null);
  const [edges, setEdges] = useState({ bottom: false, right: false });
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const update = () => {
      const bottom = el.scrollHeight - el.scrollTop - el.clientHeight > 1;
      const right = el.scrollWidth - el.scrollLeft - el.clientWidth > 1;
      setEdges((prev) => (prev.bottom === bottom && prev.right === right ? prev : { bottom, right }));
    };
    update();
    el.addEventListener("scroll", update, { passive: true });
    window.addEventListener("resize", update);
    // A resizable-panel drag changes the container size without a window resize event.
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => {
      el.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
      observer.disconnect();
    };
  }, [rows.length, columns.length, pageIndex]);

  const pageButtons = (
    <>
      <Button size="sm" variant="outline" onClick={() => table.previousPage()} disabled={!table.getCanPreviousPage()}>
        Previous
      </Button>
      <Button size="sm" variant="outline" onClick={() => table.nextPage()} disabled={!table.getCanNextPage()}>
        Next
      </Button>
    </>
  );

  return (
    <div className="space-y-2">
      <div className="relative">
        <div
          ref={scrollRef}
          className="max-h-[max(24rem,60dvh)] overflow-auto rounded-md border border-border"
        >
          <table className="w-full text-sm">
            <thead className="sticky top-0 z-10 bg-muted">
              {table.getHeaderGroups().map((hg) => (
                <tr key={hg.id}>
                  {hg.headers.map((h) => {
                    const sortDir = h.column.getIsSorted();
                    return (
                      <th
                        key={h.id}
                        aria-sort={sortDir === "asc" ? "ascending" : sortDir === "desc" ? "descending" : "none"}
                        className={cn(
                          "px-3 py-2 text-left font-medium whitespace-nowrap",
                          h.column.columnDef.meta?.numeric && "text-right",
                        )}
                      >
                        {flexRender(h.column.columnDef.header, h.getContext())}
                      </th>
                    );
                  })}
                </tr>
              ))}
            </thead>
            <tbody>
              {rows.length === 0 ? (
                <tr>
                  <td colSpan={columns.length} className="px-3 py-6 text-center text-muted-foreground">
                    No rows returned.
                  </td>
                </tr>
              ) : (
                rows.map((row) => (
                  <tr key={row.id} className="border-t border-border hover:bg-muted/30">
                    {row.getVisibleCells().map((cell) => (
                      <td
                        key={cell.id}
                        className={cn(
                          "px-3 py-1.5 whitespace-nowrap",
                          cell.column.columnDef.meta?.numeric && "text-right",
                        )}
                      >
                        {flexRender(cell.column.columnDef.cell, cell.getContext())}
                      </td>
                    ))}
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
        {edges.bottom && (
          <div className="pointer-events-none absolute inset-x-0 bottom-0 z-20 h-8 rounded-b-md bg-gradient-to-t from-background to-transparent" />
        )}
        {edges.right && (
          <div className="pointer-events-none absolute inset-y-0 right-0 z-20 w-8 bg-gradient-to-l from-background to-transparent" />
        )}
      </div>
      {showTypeBadges ? (
        totalRows > 0 && (
          <div className="flex flex-wrap items-center gap-x-4 gap-y-2 text-sm">
            <span className="text-muted-foreground">
              Showing {rangeStart.toLocaleString("en-US")}–{rangeEnd.toLocaleString("en-US")} of{" "}
              {totalRows.toLocaleString("en-US")} rows
            </span>
            <div className="flex items-center gap-2">
              <span className="text-muted-foreground">Rows per page</span>
              <Select value={String(pageSize)} onValueChange={handlePageSizeChange}>
                <SelectTrigger className="h-8 w-20">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {PAGE_SIZE_OPTIONS.map((n) => (
                    <SelectItem key={n} value={String(n)}>
                      {n}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            {pageCount > 1 && (
              <div className="flex items-center gap-2">
                <span className="text-muted-foreground">Page</span>
                <Input
                  type="number"
                  min={1}
                  max={pageCount}
                  className="h-8 w-14"
                  aria-label="Page number"
                  value={pageJumpDraft}
                  onChange={(e) => setPageJumpDraft(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") commitPageJump(e.currentTarget.value);
                  }}
                  onBlur={(e) => commitPageJump(e.target.value)}
                />
                <span className="text-muted-foreground">of {pageCount}</span>
              </div>
            )}
            <div className="ml-auto flex gap-1">
              {pageButtons}
            </div>
          </div>
        )
      ) : (
        pageCount > 1 && (
          <div className="flex items-center justify-between text-sm">
            <span className="text-muted-foreground">
              Page {pageIndex + 1} of {pageCount}
            </span>
            <div className="flex gap-1">
              {pageButtons}
            </div>
          </div>
        )
      )}
    </div>
  );
});
