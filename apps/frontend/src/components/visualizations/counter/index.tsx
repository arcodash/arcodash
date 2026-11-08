import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { formatCounter } from "@/lib/formatNumber";
import { fitFontSize } from "@/lib/fitText";
import { ColumnSelect } from "../ColumnSelect";
import type { VizEditorProps, VizRendererProps } from "../registry";

interface CounterOptions {
  columnName?: string;
  rowNumber?: number;
  countRow?: boolean;
  stringPrefix?: string;
  stringSuffix?: string;
  targetColName?: string;
  targetRowIndex?: number;
  numberGrouping?: boolean;
  numberDecimals?: number | null;
}

export function CounterRenderer({ data, options }: VizRendererProps) {
  const opts = options as CounterOptions;
  const rows = data.rows;

  const value: unknown = opts.countRow
    ? rows.length
    : rows[(opts.rowNumber ?? 1) - 1]?.[opts.columnName || data.columns[0]?.name || ""];
  const targetValue: unknown = opts.targetColName
    ? rows[opts.targetRowIndex ?? 0]?.[opts.targetColName]
    : undefined;

  const prefix = opts.stringPrefix ?? "";
  const suffix = opts.stringSuffix ?? "";

  const numVal = typeof value === "number" ? value : parseFloat(String(value));
  const numTarget = typeof targetValue === "number" ? targetValue : parseFloat(String(targetValue ?? ""));

  const fmt = { grouping: opts.numberGrouping ?? true, decimals: opts.numberDecimals };
  const formatted = formatCounter(value, fmt);

  const trend =
    targetValue !== undefined && !Number.isNaN(numVal) && !Number.isNaN(numTarget)
      ? numVal >= numTarget
        ? "up"
        : "down"
      : null;

  const hasValue = value !== null && value !== undefined;
  const text = `${hasValue ? prefix : ""}${formatted}${hasValue ? suffix : ""}`;
  // Prefix/suffix render at 0.45em, so they count for less than full digits when sizing to fit.
  const affixWidth = hasValue ? Math.ceil((prefix.length + suffix.length) * 0.45) : 0;
  const fitText = formatted + "x".repeat(affixWidth);

  return (
    <div className="flex flex-col items-center justify-center gap-2 py-8 [container-type:inline-size]">
      <div
        className="max-w-full break-words text-center font-serif font-medium leading-none tabular-nums"
        style={{ fontSize: fitFontSize(fitText, 3) }}
        title={text}
        data-testid="counter-value"
      >
        {/* Editorial style: unit marks ($, k, %) set small beside the figure. */}
        {hasValue && prefix && <span className="mr-0.5 align-top text-[0.45em]">{prefix}</span>}
        {formatted}
        {hasValue && suffix && <span className="ml-0.5 text-[0.45em]">{suffix}</span>}
      </div>
      {trend && (
        <div
          className={`flex items-center gap-1 text-sm font-medium ${
            trend === "up" ? "text-emerald-600" : "text-rose-600"
          }`}
        >
          {trend === "up" ? "↑" : "↓"} vs {formatCounter(targetValue, fmt)}
        </div>
      )}
    </div>
  );
}

export function CounterEditor({ data, options, onChange }: VizEditorProps) {
  const opts = options as CounterOptions;
  const colNames = data.columns.map((c) => c.name);

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={opts.countRow ?? false}
            className="size-4"
            onChange={(e) => onChange({ ...options, countRow: e.target.checked })}
          />
          Count rows (instead of column value)
        </label>
      </div>
      {!opts.countRow && (
        <>
          <div className="space-y-1">
            <Label>Value column</Label>
            <ColumnSelect
              value={opts.columnName ?? ""}
              onChange={(v) => onChange({ ...options, columnName: v })}
              columns={colNames}
            />
          </div>
          <div className="space-y-1">
            <Label>Row number (1-indexed)</Label>
            <Input
              type="number"
              min={1}
              value={opts.rowNumber ?? 1}
              className="w-24"
              onChange={(e) =>
                onChange({ ...options, rowNumber: Math.max(1, Number(e.target.value) || 1) })
              }
            />
          </div>
        </>
      )}
      <div className="grid grid-cols-2 gap-3">
        <div className="space-y-1">
          <Label>Prefix</Label>
          <Input
            value={opts.stringPrefix ?? ""}
            onChange={(e) => onChange({ ...options, stringPrefix: e.target.value })}
          />
        </div>
        <div className="space-y-1">
          <Label>Suffix</Label>
          <Input
            value={opts.stringSuffix ?? ""}
            onChange={(e) => onChange({ ...options, stringSuffix: e.target.value })}
          />
        </div>
      </div>
      <div className="grid grid-cols-2 gap-3">
        <div className="space-y-1">
          <Label>Decimal places</Label>
          <Select
            value={opts.numberDecimals == null ? "auto" : String(opts.numberDecimals)}
            onValueChange={(v) =>
              onChange({ ...options, numberDecimals: v === "auto" ? undefined : Number(v) })
            }
          >
            <SelectTrigger className="w-32">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="auto">Auto</SelectItem>
              {[0, 1, 2, 3, 4, 5, 6].map((d) => (
                <SelectItem key={d} value={String(d)}>
                  {d}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <label className="flex items-center gap-2 self-end pb-2 text-sm">
          <input
            type="checkbox"
            checked={opts.numberGrouping ?? true}
            className="size-4"
            onChange={(e) => onChange({ ...options, numberGrouping: e.target.checked })}
          />
          Thousands separator
        </label>
      </div>
      <div className="space-y-1">
        <Label>Target column (optional)</Label>
        <ColumnSelect
          none
          value={opts.targetColName || "none"}
          onChange={(v) => onChange({ ...options, targetColName: v === "none" ? undefined : v })}
          columns={colNames}
        />
      </div>
    </div>
  );
}

export const counterDefaultOptions: Record<string, unknown> = {
  countRow: false,
  rowNumber: 1,
  columnName: "",
  stringPrefix: "",
  stringSuffix: "",
  numberGrouping: true,
};
