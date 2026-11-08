import { useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { AlertTriangle } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ParameterControl, useDefaultParameterValues } from "@/components/query-editor/ParametersBar";
import { useDebouncedValue } from "@/lib/useDebouncedValue";
import type {
  MultiValuesOptions,
  ParamValue,
  ParameterType,
  QueryParameterDefinition,
  Widget,
} from "@/types/api";

interface Props {
  widgets: Widget[];
  onParamsChange: (params: Record<string, ParamValue>) => void;
}

interface ResolvedParam {
  name: string;
  definition?: QueryParameterDefinition;
  /** The defining widget's own query id, required to fetch a `query`-type dropdown's options; never
   * a shared/null id. */
  queryId?: number;
  conflict?: { widgetName: string; type: ParameterType; sameType: boolean };
}

function multiValuesOptionsDiverge(a?: MultiValuesOptions, b?: MultiValuesOptions): boolean {
  if (!a && !b) return false;
  if (!a || !b) return true;
  return a.prefix !== b.prefix || a.suffix !== b.suffix || a.separator !== b.separator;
}

/** Compares only behavior-affecting fields; `title`/`name`/`value` are cosmetic and must not
 * trigger a conflict warning. */
function defsDiverge(a: QueryParameterDefinition, b: QueryParameterDefinition): boolean {
  return (
    a.type !== b.type ||
    a.enum_options !== b.enum_options ||
    a.query_id !== b.query_id ||
    multiValuesOptionsDiverge(a.multi_values_options, b.multi_values_options)
  );
}

/** Resolves each referenced `{{param}}` token's definition from the first widget (list order) that
 * defines it; a later widget defining it differently is recorded as a conflict on the winner. */
export function resolveDashboardParams(widgets: Widget[]): ResolvedParam[] {
  // Map iteration follows insertion order, i.e. first reference.
  const entries = new Map<string, ResolvedParam>();

  for (const widget of widgets) {
    const tokens = widget.visualization?.query?.parameter_names;
    if (!tokens || tokens.length === 0) continue;

    const queryId = widget.visualization?.query?.id;
    const defs = widget.visualization?.query?.options?.parameters ?? [];
    const widgetName = widget.visualization?.name ?? `widget ${widget.id}`;

    for (const name of tokens) {
      let entry = entries.get(name);
      if (!entry) {
        entry = { name };
        entries.set(name, entry);
      }
      const def = defs.find((d) => d.name === name);
      if (!def || queryId == null) continue;

      if (!entry.definition) {
        entry.definition = def;
        entry.queryId = queryId;
      } else if (!entry.conflict && defsDiverge(def, entry.definition)) {
        entry.conflict = { widgetName, type: def.type, sameType: def.type === entry.definition.type };
      }
    }
  }

  return [...entries.values()];
}

const URL_PARAM_PREFIX = "p_";

export function parseValuesFromSearch(params: URLSearchParams): Record<string, ParamValue> {
  const values: Record<string, ParamValue> = {};
  for (const [key, raw] of params.entries()) {
    if (!key.startsWith(URL_PARAM_PREFIX)) continue;
    const name = key.slice(URL_PARAM_PREFIX.length);
    if (raw.startsWith("{") || raw.startsWith("[")) {
      try {
        values[name] = JSON.parse(raw);
      } catch {
        // Malformed URL state — skip this key rather than throw.
      }
    } else {
      values[name] = raw;
    }
  }
  return values;
}

export function serializeValuesToSearch(values: Record<string, ParamValue>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [name, value] of Object.entries(values)) {
    if (value === "" || value == null) continue;
    out[`${URL_PARAM_PREFIX}${name}`] =
      typeof value === "string" || typeof value === "number" ? String(value) : JSON.stringify(value);
  }
  return out;
}

export function DashboardParamsBar({ widgets, onParamsChange }: Props) {
  const resolved = useMemo(() => resolveDashboardParams(widgets), [widgets]);
  const definitions = useMemo(
    () => resolved.flatMap((r) => (r.definition ? [r.definition] : [])),
    [resolved],
  );

  const [searchParams, setSearchParams] = useSearchParams();
  // Seeded once from the URL; `useDefaultParameterValues` backstops missing names with their
  // defaults.
  const [values, setValues] = useState<Record<string, ParamValue>>(() =>
    parseValuesFromSearch(searchParams),
  );
  useDefaultParameterValues(definitions, values, (v) => setValues(v as Record<string, ParamValue>));

  // 250ms: short enough to feel responsive, long enough to skip a widget refetch per keystroke.
  const debouncedValues = useDebouncedValue(values, 250);

  useEffect(() => {
    onParamsChange(debouncedValues);
  }, [debouncedValues, onParamsChange]);

  useEffect(() => {
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        for (const key of [...next.keys()]) if (key.startsWith(URL_PARAM_PREFIX)) next.delete(key);
        for (const [key, value] of Object.entries(serializeValuesToSearch(debouncedValues))) next.set(key, value);
        return next;
      },
      { replace: true },
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [debouncedValues]);

  if (resolved.length === 0) return null;

  function handleChange(name: string, value: unknown) {
    setValues((prev) => ({ ...prev, [name]: value as ParamValue }));
  }

  return (
    <div className="flex flex-wrap items-end gap-3 rounded-md border border-border bg-muted/30 p-3">
      {resolved.map((entry) => {
        if (!entry.definition || entry.queryId == null) {
          return (
            <div key={entry.name} className="flex flex-col gap-1">
              <Label className="text-xs text-muted-foreground">{entry.name}</Label>
              <Input
                className="h-8 w-40"
                disabled
                placeholder={entry.name}
                title="Configure this parameter in the query editor."
              />
            </div>
          );
        }
        return (
          <div key={entry.name} className="flex items-end gap-1">
            <ParameterControl
              definition={entry.definition}
              value={values[entry.name]}
              onChange={(v) => handleChange(entry.name, v)}
              queryId={entry.queryId}
              testId={`dashboard-param-${entry.name}`}
            />
            {entry.conflict && (
              <span
                className="mb-1.5 flex h-4 w-4 shrink-0 items-center justify-center text-warning-foreground"
                title={
                  entry.conflict.sameType
                    ? `Also defined with different options in widget '${entry.conflict.widgetName}'; using the first definition.`
                    : `Also defined as ${entry.conflict.type} in widget '${entry.conflict.widgetName}'; using the first definition.`
                }
              >
                <AlertTriangle className="h-4 w-4" aria-hidden="true" />
              </span>
            )}
          </div>
        );
      })}
    </div>
  );
}
