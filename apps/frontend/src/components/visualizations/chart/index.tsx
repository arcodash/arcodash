import { useMemo, type ReactNode } from "react";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  Label as ChartLabel,
  Legend,
  Line,
  LineChart,
  Pie,
  PieChart,
  ResponsiveContainer,
  Scatter,
  ScatterChart,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { axisTick, formatNumeric, isNumericType, NUMBER_LOCALE } from "@/lib/formatNumber";
import { computeDateGranularity, formatDateValue } from "@/lib/formatDate";
import { ColumnSelect } from "../ColumnSelect";
import type { VizEditorProps, VizRendererProps } from "../registry";

// Editorial navy family, light/dark alternating so neighbours stay apart; orange, gray and teal as accents.
// Every entry is >= 3:1 against white (WCAG 1.4.11 for graphical marks).
export const PALETTE = [
  "#3d5a98", "#7593c6", "#1e3768", "#c46f1c",
  "#5e7fb8", "#8c8c8c", "#2c4a80", "#3f7f7a",
];
export const INK = "#171717";
export const INK_MUTED = "#525252";
const GRID = "#e5e5e5";
const AXIS_STROKE = "#a3a3a3";
const AXIS_TICK = { fontSize: 12, fill: INK_MUTED };
const TOOLTIP_CONTENT = { borderColor: GRID, borderRadius: 6, fontSize: 12 };
// Recharts paints legend text in the series colour; force readable ink instead.
const legendLabel = (value: unknown) => <span style={{ color: INK_MUTED }}>{String(value)}</span>;
const LEGEND = {
  verticalAlign: "top",
  align: "left",
  iconSize: 10,
  wrapperStyle: { fontSize: 12, paddingBottom: 8 },
  formatter: legendLabel,
} as const;

// Direct slice labels need room: skip them for crowded or small pies (the tooltip/legend still name slices).
const PIE_LABEL_MAX_SLICES = 8;
// Labels sit ~outerRadius + 20px out; the 70% ring leaves ~30% of the radius clear for them, and
// below this radius they still collide with the legend or SVG edge.
const PIE_LABEL_MIN_RADIUS = 70;
const PIE_LABEL_MIN_PERCENT = 0.05;
// ~width of one 12px Inter glyph; enough to decide whether a label fits beside the ring.
const PIE_LABEL_CHAR_PX = 6.8;

/** Widest slice label that fits between its anchor and the chart edge: "NN% name", then "NN%",
 * then nothing. The pie is centred, so the free width on the anchor side is x or 2*cx - x. */
export function fitPieLabel(pct: string, name: string, room: number): string | null {
  for (const text of [`${pct} ${name}`, pct]) {
    if (text.length * PIE_LABEL_CHAR_PX <= room) return text;
  }
  return null;
}

/** Donut centre figure, kept short enough for the hole: compact past 100k, no decimals past 1k, and
 * never float noise. */
export function formatPieTotal(v: number): string {
  const abs = Math.abs(v);
  return new Intl.NumberFormat(
    NUMBER_LOCALE,
    abs >= 1e5 ? { notation: "compact", maximumFractionDigits: 1 } : { maximumFractionDigits: abs >= 1e3 ? 0 : 2 },
  ).format(v);
}

const CHART_TYPES = ["line", "bar", "area", "pie", "scatter"] as const;
type ChartType = (typeof CHART_TYPES)[number];

const AGGREGATIONS = ["none", "sum", "avg", "min", "max", "count"] as const;
type Aggregation = (typeof AGGREGATIONS)[number];
const AGGREGATION_LABELS: Record<Aggregation, string> = {
  none: "Don't combine",
  sum: "Sum",
  avg: "Average",
  min: "Minimum",
  max: "Maximum",
  count: "Count of rows",
};

interface ChartOptions {
  globalSeriesType?: ChartType;
  xColumn?: string;
  yColumns?: string[];
  // Set once the Y-axis picker is touched, so an empty selection differs from "never configured".
  yColumnsTouched?: boolean;
  seriesColumn?: string;
  aggregation?: Aggregation;
  legend?: boolean;
  stacking?: "normal" | "percent" | "";
}

// Shared by renderer and editor so the dialog reflects the inferred defaults.
function resolveXCol(columns: { name: string; type: string }[], opts: ChartOptions): string {
  const currentColumnNames = new Set(columns.map((c) => c.name));
  if (opts.xColumn && currentColumnNames.has(opts.xColumn)) return opts.xColumn;
  // Prefer a non-numeric column for the axis; use the first only when all are numeric.
  const firstNonNumeric = columns.find((c) => !isNumericType(c.type));
  return firstNonNumeric?.name ?? columns[0]?.name ?? "";
}

const CONTINUOUS_X_TYPES = new Set(["date", "datetime"]);

// Line implies a continuous axis; for plain string categories it falsely implies trend, so default
// to bar.
function resolveChartType(columns: { name: string; type: string }[], xCol: string, opts: ChartOptions): ChartType {
  if (opts.globalSeriesType) return opts.globalSeriesType;
  const xColType = columns.find((c) => c.name === xCol)?.type;
  return xColType && CONTINUOUS_X_TYPES.has(xColType) ? "line" : "bar";
}

function resolveYCols(
  columns: { name: string; type: string }[],
  opts: ChartOptions,
  xCol: string,
): string[] {
  const currentColumnNames = new Set(columns.map((c) => c.name));
  const saved = (opts.yColumns ?? []).filter((c) => currentColumnNames.has(c));
  // Once touched, an empty array is a deliberate "no series" choice, not a missing config.
  if (opts.yColumnsTouched || saved.length > 0) return saved;
  // Never invent a series from a string column.
  const firstNumeric = columns.find((c) => isNumericType(c.type) && c.name !== xCol);
  return firstNumeric ? [firstNumeric.name] : [];
}

/** Postgres NUMERIC arrives as a JSON string (see crates/shared/src/connectors/postgres.rs); Recharts
 * needs numbers, so coerce the plotted columns. */
function coerceNumericCols(
  rows: Record<string, unknown>[],
  numericCols: string[],
): Record<string, unknown>[] {
  if (numericCols.length === 0) return rows;
  return rows.map((r) => {
    const out = { ...r };
    for (const col of numericCols) {
      const v = out[col];
      if (typeof v === "string" && v.trim() !== "") {
        const n = Number(v);
        if (!Number.isNaN(n)) out[col] = n;
      }
    }
    return out;
  });
}

/** `raw` is `null` for non-numeric rows: `count` counts all, but sum/avg/min/max must drop them,
 * not treat NULL as zero. */
function combine(raw: (number | null)[], agg: Aggregation): number {
  if (agg === "count") return raw.length;
  const values = raw.filter((v): v is number => v !== null);
  if (values.length === 0) return 0;
  if (agg === "avg") return values.reduce((a, b) => a + b, 0) / values.length;
  // Spreading a large bucket into Math.min/max throws RangeError.
  if (agg === "min") return values.reduce((a, b) => Math.min(a, b));
  if (agg === "max") return values.reduce((a, b) => Math.max(a, b));
  return values.reduce((a, b) => a + b, 0);
}

/** Number, or null for non-numbers (incl. SQL NULL): `Number(null)` is 0 and would corrupt
 * avg/min/max. */
function toAggregable(v: unknown): number | null {
  if (v === null || v === undefined) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

/** No group-by: combine rows per X value, per Y column; order follows first appearance, not a sort. */
export function aggregateByX(
  rows: Record<string, unknown>[],
  xCol: string,
  yCols: string[],
  agg: Aggregation,
): Record<string, unknown>[] {
  // Map iteration follows insertion order, i.e. first appearance of each X value.
  const buckets = new Map<string, Record<string, (number | null)[]>>();
  for (const r of rows) {
    const xKey = String(r[xCol] ?? "");
    let bucket = buckets.get(xKey);
    if (!bucket) buckets.set(xKey, (bucket = {}));
    for (const y of yCols) (bucket[y] ??= []).push(toAggregable(r[y]));
  }
  return [...buckets].map(([xKey, bucket]) => {
    const out: Record<string, unknown> = { [xCol]: xKey };
    for (const y of yCols) out[y] = combine(bucket[y] ?? [], agg);
    return out;
  });
}

/** Group-by: pivot one value column into a series per group value; rows sharing (x, group) combine
 * per `agg`. */
export function pivotBySeries(
  rows: Record<string, unknown>[],
  xCol: string,
  valueCol: string,
  seriesCol: string,
  agg: Aggregation,
): { rows: Record<string, unknown>[]; seriesNames: string[] } {
  // Map/Set iteration follows insertion order, i.e. first appearance of each X and group value.
  const seriesNames = new Set<string>();
  // Group values are data, not column names: a Map keeps "constructor"/"__proto__" safe as keys.
  const buckets = new Map<string, Map<string, (number | null)[]>>();
  for (const r of rows) {
    const xKey = String(r[xCol] ?? "");
    const sKey = String(r[seriesCol] ?? "");
    let seriesMap = buckets.get(xKey);
    if (!seriesMap) buckets.set(xKey, (seriesMap = new Map()));
    let values = seriesMap.get(sKey);
    if (!values) seriesMap.set(sKey, (values = []));
    values.push(toAggregable(r[valueCol]));
    seriesNames.add(sKey);
  }
  const pivotedRows = [...buckets].map(([xKey, seriesMap]) => {
    const out: Record<string, unknown> = { [xCol]: xKey };
    for (const sKey of seriesNames) out[sKey] = combine(seriesMap.get(sKey) ?? [], agg);
    return out;
  });
  return { rows: pivotedRows, seriesNames: [...seriesNames] };
}

function ChartEmptyState({ title, body }: { title: string; body: string }) {
  return (
    // No min-height: this renders in a 200px mobile widget slot and must fit its parent's box.
    <div className="flex h-full flex-col items-center justify-center gap-1 overflow-hidden px-6 text-center">
      <p className="text-sm font-medium text-foreground">{title}</p>
      <p className="max-w-[34ch] text-xs text-muted-foreground">{body}</p>
    </div>
  );
}

/** Chart name above the plot only when the host has no card header. The `min-h-0 flex-1` wrapper is
 * load-bearing: without it a `height="100%"` ResponsiveContainer resolves to 0. */
function withTitle(title: string | undefined, children: ReactNode) {
  if (!title) return children;
  return (
    <div className="flex h-full flex-col">
      <p className="mb-2 shrink-0 font-serif text-base font-medium text-foreground">{title}</p>
      <div className="min-h-0 flex-1">{children}</div>
    </div>
  );
}

const numericTooltipFormatter = (v: number | string | readonly (number | string)[] | undefined) =>
  typeof v === "number" || typeof v === "string" ? formatNumeric(v) : (v ?? "");

export function ChartRenderer({ data, options, title }: VizRendererProps) {
  const opts = options as ChartOptions;
  const xCol = resolveXCol(data.columns, opts);
  const yCols = resolveYCols(data.columns, opts, xCol);
  const chartType = resolveChartType(data.columns, xCol, opts);
  const canAggregate = chartType === "line" || chartType === "bar" || chartType === "area";

  const hasNumericColumn = data.columns.some((c) => isNumericType(c.type));

  const rawRows = data.rows as Record<string, unknown>[];
  const numericCols = chartType === "scatter" ? [xCol, ...yCols] : yCols;
  const coercedRows = useMemo(
    () => coerceNumericCols(rawRows, numericCols),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [rawRows, numericCols.join(",")],
  );

  const seriesCol =
    canAggregate && opts.seriesColumn && opts.seriesColumn !== xCol ? opts.seriesColumn : "";
  const grouped = seriesCol !== "" && data.columns.some((c) => c.name === seriesCol);
  // Grouping without aggregation is ambiguous for a shared (x, series) pair; force "sum" (no-op on
  // unique data).
  const aggregation: Aggregation = grouped
    ? opts.aggregation && opts.aggregation !== "none"
      ? opts.aggregation
      : "sum"
    : (opts.aggregation ?? "none");

  const pivoted = useMemo(() => {
    if (!canAggregate || !xCol || yCols.length === 0) return null;
    if (grouped) return pivotBySeries(coercedRows, xCol, yCols[0], seriesCol, aggregation);
    if (aggregation !== "none") return { rows: aggregateByX(coercedRows, xCol, yCols, aggregation), seriesNames: yCols };
    return null;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canAggregate, coercedRows, xCol, yCols.join(","), grouped, seriesCol, aggregation]);

  const rows = pivoted ? pivoted.rows : coercedRows;
  const plottedCols = pivoted ? pivoted.seriesNames : yCols;

  // axisTick has no date branch; a raw ISO timestamp would fail numeric formatting.
  const xColType = data.columns.find((c) => c.name === xCol)?.type;
  const xIsDate = !!xColType && CONTINUOUS_X_TYPES.has(xColType);
  const xTickFormatter = useMemo(() => {
    if (!xIsDate) return axisTick;
    const granularity = computeDateGranularity(rows.map((r) => r[xCol]));
    return (v: unknown) => formatDateValue(v, granularity);
  }, [xIsDate, rows, xCol]);

  const empty = (heading: string, body: string) => withTitle(title, <ChartEmptyState title={heading} body={body} />);
  if (data.columns.length === 0) return empty("No columns in this result", "The query returned no columns to plot.");
  if (rawRows.length === 0) {
    return empty("No rows to plot", "The query returned no rows. Run it again or widen the filters.");
  }
  if (yCols.length === 0) {
    return hasNumericColumn
      ? empty("Pick a column to plot", "Open the editor and tick at least one Y-axis column.")
      : empty(
          "No number column to plot",
          "A chart needs at least one number column. This result has none — use a Table visualization instead.",
        );
  }

  const stackOffset = opts.stacking === "percent" ? "expand" : undefined;
  const useStack = !!opts.stacking;
  const showLegend = opts.legend !== false;

  if (chartType === "pie") {
    let hasNegative = false;
    // Zero/negative rows have no angle: drop them so arcs, legend, tooltip and centre total agree.
    const pieData = rows.flatMap((r) => {
      const raw = Number(r[yCols[0]] ?? 0);
      if (raw < 0) hasNegative = true;
      return Number.isFinite(raw) && raw > 0 ? [{ name: String(r[xCol] ?? ""), value: raw }] : [];
    });
    const pieTotal = pieData.reduce((sum, d) => sum + d.value, 0);
    if (pieTotal === 0) {
      return empty("Nothing to plot", hasNegative ? "A pie chart needs positive values." : "Every value in this column is zero.");
    }
    const showSliceLabels = pieData.length <= PIE_LABEL_MAX_SLICES;
    return withTitle(
      title,
      <ResponsiveContainer width="100%" height="100%">
        <PieChart>
          <Pie
            data={pieData}
            dataKey="value"
            nameKey="name"
            // Leave room around the ring for direct labels when they're on.
            innerRadius={showSliceLabels ? "42%" : "48%"}
            outerRadius={showSliceLabels ? "70%" : "80%"}
            stroke="#fff"
            strokeWidth={2}
            labelLine={false}
            label={
              showSliceLabels
                ? ({ x, y, cx, textAnchor, name, percent, outerRadius }) => {
                    if ((percent ?? 0) < PIE_LABEL_MIN_PERCENT || Number(outerRadius) < PIE_LABEL_MIN_RADIUS) return null;
                    const lx = Number(x);
                    const room = (textAnchor === "end" ? lx : 2 * Number(cx) - lx) - 4;
                    const text = fitPieLabel(`${Math.round((percent ?? 0) * 100)}%`, axisTick(name), room);
                    return text === null ? null : (
                      <text x={lx} y={y} textAnchor={textAnchor} dominantBaseline="central" fontSize={12} fill={INK_MUTED}>
                        {text}
                      </text>
                    );
                  }
                : false
            }
            isAnimationActive={false}
          >
            {pieData.map((_, i) => (
              <Cell key={i} fill={PALETTE[i % PALETTE.length]} />
            ))}
            <ChartLabel
              position="center"
              content={({ viewBox }) => {
                // Recharts 3 hands a Label inside Pie the plot-area viewBox; its centre is the pie
                // centre (default cx/cy).
                const { x = 0, y = 0, width = 0, height = 0 } = (viewBox ?? {}) as {
                  x?: number; y?: number; width?: number; height?: number;
                };
                // Hole is ~half the radius; scale the figure with it and drop it when there's no room.
                const size = Math.min(20, Math.min(width, height) * 0.09);
                if (size < 10) return null;
                const cx = x + width / 2;
                return (
                  <text x={cx} y={y + height / 2} textAnchor="middle">
                    <tspan x={cx} dy="-0.1em" fontSize={size} fontWeight={600} fill={INK}>{formatPieTotal(pieTotal)}</tspan>
                    <tspan x={cx} dy="1.4em" fontSize={Math.min(12, size * 0.65)} fill={INK_MUTED}>Total</tspan>
                  </text>
                );
              }}
            />
          </Pie>
          {showLegend && <Legend {...LEGEND} />}
          <Tooltip formatter={numericTooltipFormatter} contentStyle={TOOLTIP_CONTENT} />
        </PieChart>
      </ResponsiveContainer>,
    );
  }

  if (chartType === "scatter") {
    const scatterXNumeric = isNumericType(data.columns.find((c) => c.name === xCol)?.type ?? "");
    return withTitle(
      title,
      <ResponsiveContainer width="100%" height="100%">
        <ScatterChart>
          <CartesianGrid vertical={false} stroke={GRID} />
          <XAxis
            dataKey={xCol}
            name={xCol}
            // A category X axis repeats ticks once per Scatter series; numeric X needs a number axis.
            type={scatterXNumeric ? "number" : "category"}
            // The default number domain starts at 0, which crushes X values like years to one edge.
            domain={scatterXNumeric ? ["auto", "auto"] : undefined}
            tickFormatter={axisTick}
            tick={AXIS_TICK}
            tickLine={false}
            stroke={AXIS_STROKE}
          />
          <YAxis
            width="auto"
            orientation="right"
            axisLine={false}
            tickLine={false}
            tick={AXIS_TICK}
            tickFormatter={(v: number) => formatNumeric(v)}
          />
          <Tooltip cursor={{ strokeDasharray: "3 3" }} contentStyle={TOOLTIP_CONTENT} formatter={numericTooltipFormatter} />
          {showLegend && <Legend {...LEGEND} />}
          {yCols.map((yCol, i) => (
            <Scatter
              key={yCol}
              name={yCol}
              data={rows}
              fill={PALETTE[i % PALETTE.length]}
              dataKey={yCol}
              isAnimationActive={false}
            />
          ))}
        </ScatterChart>
      </ResponsiveContainer>,
    );
  }

  const ChartMap = { line: LineChart, bar: BarChart, area: AreaChart } as const;
  const ChartComp = ChartMap[chartType as keyof typeof ChartMap] ?? LineChart;

  return withTitle(
    title,
    <ResponsiveContainer width="100%" height="100%">
      <ChartComp data={rows} stackOffset={stackOffset} barCategoryGap="22%">
        <CartesianGrid vertical={false} stroke={GRID} />
        <XAxis
          dataKey={xCol}
          interval="preserveStartEnd"
          minTickGap={16}
          tickMargin={8}
          height={28}
          tickLine={false}
          stroke={AXIS_STROKE}
          tick={AXIS_TICK}
          tickFormatter={xTickFormatter}
        />
        <YAxis
          width="auto"
          orientation="right"
          axisLine={false}
          tickLine={false}
          tick={AXIS_TICK}
          tickFormatter={
            stackOffset === "expand" ? (v: number) => `${(v * 100).toFixed(0)}%` : (v: number) => formatNumeric(v)
          }
        />
        <Tooltip
          formatter={numericTooltipFormatter}
          labelFormatter={xIsDate ? xTickFormatter : undefined}
          contentStyle={TOOLTIP_CONTENT}
          cursor={chartType === "bar" ? { fill: "#f5f5f5" } : { stroke: AXIS_STROKE }}
        />
        {showLegend && <Legend {...LEGEND} />}
        {plottedCols.map((key, i) => {
          const color = PALETTE[i % PALETTE.length];
          // Ungrouped "count" plots under every Y key; the legend/tooltip must say "Count", not a
          // column name.
          const name = !grouped && aggregation === "count" ? "Count" : key;
          // Static draw: rAF-driven mount animations freeze mid-way in throttled tabs and captures.
          const still = { isAnimationActive: false } as const;
          if (chartType === "area")
            return <Area key={key} type="monotone" dataKey={key} name={name} stroke={color} fill={color} fillOpacity={0.25} strokeWidth={2} stackId={useStack ? "stack" : undefined} {...still} />;
          if (chartType === "bar")
            return <Bar key={key} dataKey={key} name={name} fill={color} stackId={useStack ? "stack" : undefined} {...still} />;
          return <Line key={key} type="linear" dataKey={key} name={name} stroke={color} strokeWidth={2.5} dot={rows.length <= 24 ? { r: 3, fill: color, strokeWidth: 0 } : false} {...still} />;
        })}
      </ChartComp>
    </ResponsiveContainer>,
  );
}

export function ChartEditor({ data, options, onChange }: VizEditorProps) {
  const opts = options as ChartOptions;
  const colNames = data.columns.map((c) => c.name);
  const xCol = resolveXCol(data.columns, opts);
  const yCols = resolveYCols(data.columns, opts, xCol);
  const numericColNames = data.columns.filter((c) => isNumericType(c.type)).map((c) => c.name);

  // Group-by candidates: columns that are not already X or Y, non-numeric first.
  const seriesCandidates = data.columns.filter((c) => c.name !== xCol && !yCols.includes(c.name));
  const groupByColumns = [
    ...seriesCandidates.filter((c) => !isNumericType(c.type)),
    ...seriesCandidates.filter((c) => isNumericType(c.type)),
  ].map((c) => c.name);

  // Pie and scatter never aggregate, so Group by / Combine rows are hidden.
  const chartType = resolveChartType(data.columns, xCol, opts);
  const canAggregate = chartType === "line" || chartType === "bar" || chartType === "area";
  const canStack = chartType === "bar" || chartType === "area";

  const seriesColumn = opts.seriesColumn ?? "";
  const grouped = canAggregate && seriesColumn !== "";
  const aggregation = opts.aggregation ?? "none";
  const combineOptions = grouped ? AGGREGATIONS.filter((a) => a !== "none") : AGGREGATIONS;

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <Label>Chart type</Label>
        <Select value={chartType} onValueChange={(v) => onChange({ ...options, globalSeriesType: v })}>
          <SelectTrigger className="w-40"><SelectValue /></SelectTrigger>
          <SelectContent>
            {CHART_TYPES.map((t) => (
              <SelectItem key={t} value={t}>{t.charAt(0).toUpperCase() + t.slice(1)}</SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="space-y-1">
        <Label>X-axis column</Label>
        <ColumnSelect
          value={xCol}
          // Drop the new x-axis column from yColumns too, or it'd plot against itself.
          onChange={(v) => onChange({ ...options, xColumn: v, yColumns: yCols.filter((c) => c !== v) })}
          columns={colNames}
        />
      </div>
      {canAggregate && (
        <div className="space-y-1">
          <Label>Group by</Label>
          <ColumnSelect
            none
            value={seriesColumn || "none"}
            onChange={(v) => {
              const nextSeriesColumn = v === "none" ? "" : v;
              const patch: Record<string, unknown> = { ...options, seriesColumn: nextSeriesColumn };
              // Grouping without aggregation is ambiguous for a shared (x, group) pair; force Sum.
              if (nextSeriesColumn && (!opts.aggregation || opts.aggregation === "none")) {
                patch.aggregation = "sum";
              }
              // A group-by plots one value column (see pivotBySeries); keep the first checked.
              if (nextSeriesColumn && yCols.length > 1) patch.yColumns = [yCols[0]];
              onChange(patch);
            }}
            columns={groupByColumns}
          />
          <p className="mt-1 text-xs text-muted-foreground">
            One line per value in this column. Leave as None for a single series.
          </p>
        </div>
      )}
      {canAggregate && (
        <div className="space-y-1">
          <Label>Combine rows</Label>
          <Select
            value={aggregation}
            onValueChange={(v) => onChange({ ...options, aggregation: v })}
          >
            <SelectTrigger className="w-40"><SelectValue /></SelectTrigger>
            <SelectContent>
              {combineOptions.map((a) => (
                <SelectItem key={a} value={a}>{AGGREGATION_LABELS[a]}</SelectItem>
              ))}
            </SelectContent>
          </Select>
          <p className="mt-1 text-xs text-muted-foreground">
            {grouped
              ? "Rows that share an X value and a group are combined this way."
              : "Rows that share an X value are combined this way."}
          </p>
        </div>
      )}
      <div className="space-y-1">
        <Label>Y-axis columns</Label>
        <div className="space-y-1">
          {numericColNames.filter((c) => c !== xCol).map((c) => {
            const checked = yCols.includes(c);
            return (
              <label key={c} className="flex items-center gap-2 text-sm">
                <input
                  type={grouped ? "radio" : "checkbox"}
                  name={grouped ? "y-axis-column" : undefined}
                  checked={checked}
                  className="size-4"
                  onChange={(e) =>
                    onChange({
                      ...options,
                      // Grouped: only one value column can be plotted, so a checkbox would accept
                      // ticks the renderer ignores.
                      yColumns: grouped ? [c] : e.target.checked ? [...yCols, c] : yCols.filter((x) => x !== c),
                      yColumnsTouched: true,
                    })
                  }
                />
                {c}
              </label>
            );
          })}
        </div>
        <p className="mt-1 text-xs text-muted-foreground">
          {grouped
            ? "Pick one column to plot per group when grouping by X."
            : "Only number columns can be plotted."}
        </p>
      </div>
      {canStack && (
        <div className="space-y-1">
          <Label>Stacking</Label>
          <Select
            value={opts.stacking || "none"}
            onValueChange={(v) => onChange({ ...options, stacking: v === "none" ? "" : v })}
          >
            <SelectTrigger className="w-40"><SelectValue placeholder="None" /></SelectTrigger>
            <SelectContent>
              <SelectItem value="none">None</SelectItem>
              <SelectItem value="normal">Stacked</SelectItem>
              <SelectItem value="percent">100%</SelectItem>
            </SelectContent>
          </Select>
        </div>
      )}
      <label className="flex items-center gap-2 text-sm">
        <input type="checkbox" checked={opts.legend !== false} className="size-4"
          onChange={(e) => onChange({ ...options, legend: e.target.checked })} />
        Show legend
      </label>
    </div>
  );
}

export const chartDefaultOptions: Record<string, unknown> = {
  // No globalSeriesType: unset lets resolveChartType pick line vs bar from the data shape.
  xColumn: "",
  yColumns: [],
  seriesColumn: "",
  aggregation: "none",
  legend: true,
  stacking: "",
};
