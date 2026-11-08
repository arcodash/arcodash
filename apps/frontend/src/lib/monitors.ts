import { formatNumeric } from "@/lib/formatNumber";
import { api, apiErrorMessage } from "@/lib/api";
import type {
  MonitorKind,
  MonitorOptions,
  QueryMonitorSuggestion,
  ResultColumn,
  Sensitivity,
} from "@/types/api";

export type { MonitorKind };

/** The comparison operators a monitor's `options.op` accepts (mirrors backend
 * `monitor_service::validate_options`). */
export const OPS = [">", "<", ">=", "<=", "==", "!="] as const;

/** Plain-language labels over the worker's numeric comparison
 * (`evaluate_monitor.rs::evaluate_state`). */
export const OP_LABELS: Record<(typeof OPS)[number], { phrase: string; symbol: string }> = {
  ">": { phrase: "is greater than", symbol: ">" },
  ">=": { phrase: "is at least", symbol: "≥" },
  "<": { phrase: "is less than", symbol: "<" },
  "<=": { phrase: "is at most", symbol: "≤" },
  "==": { phrase: "equals", symbol: "=" },
  "!=": { phrase: "does not equal", symbol: "≠" },
};

/** Friendly types the worker's `cell_as_f64` can parse; other types always evaluate to
 * "unevaluable". */
const NUMERIC_FRIENDLY_TYPES = new Set(["integer", "float"]);

/** Whether a condition on this column could fire. A missing `friendly_type` counts as comparable so a
 * save validates it instead of silently hiding the column. */
export function isComparableColumn(column: Pick<ResultColumn, "friendly_type">): boolean {
  return !column.friendly_type || NUMERIC_FRIENDLY_TYPES.has(column.friendly_type);
}

export function opLabel(op: string): { phrase: string; symbol: string } {
  return OP_LABELS[op as keyof typeof OP_LABELS] ?? OP_LABELS[">"];
}

export function formatComparisonValue(value: string): string {
  const n = Number(value);
  return Number.isFinite(n) ? formatNumeric(n) : value;
}

export const SENSITIVITIES = ["high", "medium", "low"] as const satisfies readonly Sensitivity[];

/** `flatPct` mirrors the worker's flat-series floor on near-constant series, so the hint never
 * claims "any change" alerts. */
export const SENSITIVITY_LABELS: Record<Sensitivity, { label: string; hint: string; flatPct: number }> = {
  high: { label: "High", hint: "more alerts", flatPct: 37 },
  medium: { label: "Medium", hint: "recommended", flatPct: 52 },
  low: { label: "Low", hint: "fewer alerts", flatPct: 74 },
};

export function median(values: number[]): number | null {
  if (values.length === 0) return null;
  const s = [...values].sort((a, b) => a - b);
  const mid = s.length >> 1;
  return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
}

const AI_MARKER = "\n\nAI analysis (confidence: ";

export function splitExplanation(text: string): { stats: string; ai?: string; confidence?: Sensitivity } {
  const i = text.indexOf(AI_MARKER);
  if (i < 0) return { stats: text };
  const m = /^(low|medium|high)\): ([\s\S]*)$/.exec(text.slice(i + AI_MARKER.length));
  if (!m) return { stats: text };
  return { stats: text.slice(0, i), ai: m[2], confidence: m[1] as Sensitivity };
}

/** What a create chain has written so far. `workingVersion` is present only while the query is created but
 * unpublished; resume keys on it so it never reaches `createMonitor` for an unpublished query. */
export type PartialCheck = { queryId?: number; workingVersion?: number; monitorId?: number };

/** Carries what a failed chain already wrote so a retry can `resume` instead of minting duplicates. */
export class PartialCheckError extends Error {
  partial: PartialCheck;

  constructor(message: string, partial: PartialCheck, cause?: unknown) {
    super(message, cause !== undefined ? { cause } : undefined);
    this.partial = partial;
  }
}

/** Create query, publish, create monitor, subscribe creator. A subscription failure is swallowed
 * (`subscribed: false`); any other failure throws `PartialCheckError` for `resume`. */
export async function createMonitorFromSql(
  input: {
    dataSourceId: number;
    queryName: string;
    description?: string;
    sql: string;
    tags?: string[];
    monitorName: string;
    options: MonitorOptions;
    rearm?: number;
    subscribe?: { destinationId: number | null } | false;
  },
  resume: PartialCheck = {},
): Promise<{ queryId: number; monitorId: number; subscribed: boolean }> {
  let queryId = resume.queryId;
  let workingVersion = resume.workingVersion;
  let monitorId = resume.monitorId;
  try {
    if (queryId == null) {
      const query = await api.createQuery({
        data_source_id: input.dataSourceId,
        name: input.queryName,
        description: input.description,
        query: input.sql,
        tags: input.tags,
      });
      queryId = query.id;
      workingVersion = query.working_version;
    }
    if (workingVersion != null) {
      await api.publishQuery(queryId, workingVersion);
      workingVersion = undefined;
    }
    if (monitorId == null) {
      const monitor = await api.createMonitor({
        query_id: queryId,
        name: input.monitorName,
        options: input.options,
        ...(input.rearm != null && { rearm: input.rearm }),
      });
      monitorId = monitor.id;
    }
  } catch (err) {
    throw new PartialCheckError(
      apiErrorMessage(err, "Failed to create check"),
      { queryId, workingVersion, monitorId },
      err,
    );
  }
  let subscribed = false;
  if (input.subscribe !== false) {
    try {
      await api.createMonitorSubscription(monitorId, { destination_id: input.subscribe?.destinationId ?? null });
      subscribed = true;
    } catch {
      /* the monitor exists; the caller reports subscribed: false */
    }
  }
  return { queryId, monitorId, subscribed };
}

export async function createMonitorOnQuery(
  queryId: number,
  name: string,
  options: MonitorOptions,
): Promise<{ monitorId: number; subscribed: boolean }> {
  const monitorId = (await api.createMonitor({ query_id: queryId, name, options })).id;
  let subscribed = true;
  try {
    await api.createMonitorSubscription(monitorId, { destination_id: null });
  } catch {
    subscribed = false;
  }
  return { monitorId, subscribed };
}

export function suggestionOptions(s: QueryMonitorSuggestion): MonitorOptions {
  return s.kind === "baseline"
    ? { kind: "baseline", column: s.column, sensitivity: s.sensitivity }
    : { kind: "threshold", column: s.column, op: s.op, value: s.value };
}
