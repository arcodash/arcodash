import { Label } from "@/components/ui/label";
import { ColumnSelect } from "../ColumnSelect";
import type { VizEditorProps, VizRendererProps } from "../registry";

interface FunnelOptions {
  column?: string;
  valueColumn?: string;
}

const COLORS = ["#3b82f6", "#6366f1", "#8b5cf6", "#a855f7", "#c084fc", "#d946ef", "#ec4899"];

/** Largest value, floored at 0 and falling back to 1 so ratios never divide by zero. */
export function funnelMax(values: number[]): number {
  // reduce, not Math.max(...values): spreading a large result set throws RangeError.
  return values.reduce((a, v) => (v > a ? v : a), 0) || 1;
}

export function FunnelRenderer({ data, options }: VizRendererProps) {
  const opts = options as FunnelOptions;
  const labelCol = opts.column || data.columns[0]?.name || "";
  const valueCol = opts.valueColumn || data.columns[1]?.name || "";

  const rows = data.rows.map((r) => {
    const raw = r[valueCol];
    const parsed = typeof raw === "number" ? raw : parseFloat(String(raw ?? 0));
    return {
      label: String(r[labelCol] ?? ""),
      // A non-numeric value column parses to NaN, which would render as literal "NaN" text
      // and a `width: NaN%` inline style — fall back to 0 so the bar collapses instead.
      value: Number.isNaN(parsed) ? 0 : parsed,
    };
  });

  const max = funnelMax(rows.map((r) => r.value));

  return (
    <div className="space-y-1 py-4">
      {rows.map((row, i) => {
        const pct = Math.min(100, Math.max(0, Math.round((row.value / max) * 100)));
        return (
          <div key={i} className="flex items-center gap-3">
            <div className="w-32 truncate text-right text-sm text-muted-foreground">
              {row.label}
            </div>
            <div className="flex-1">
              <div
                className="h-8 rounded-sm flex items-center px-2 text-sm font-medium text-white"
                style={{
                  width: `${pct}%`,
                  minWidth: "3rem",
                  backgroundColor: COLORS[i % COLORS.length],
                }}
              >
                {row.value.toLocaleString()}
              </div>
            </div>
            <div className="w-12 text-right text-xs text-muted-foreground">{pct}%</div>
          </div>
        );
      })}
    </div>
  );
}

export function FunnelEditor({ data, options, onChange }: VizEditorProps) {
  const opts = options as FunnelOptions;
  const colNames = data.columns.map((c) => c.name);
  const fields = [
    ["column", "Label column"],
    ["valueColumn", "Value column"],
  ] as const;

  return (
    <div className="space-y-4">
      {fields.map(([key, label]) => (
        <div key={key} className="space-y-1">
          <Label>{label}</Label>
          <ColumnSelect
            value={opts[key] ?? ""}
            onChange={(v) => onChange({ ...options, [key]: v })}
            columns={colNames}
          />
        </div>
      ))}
    </div>
  );
}

export const funnelDefaultOptions: Record<string, unknown> = {
  column: "",
  valueColumn: "",
};
