import type { ParamValue } from "@/types/api";

function isCompleteParamValue(value: ParamValue | undefined): boolean {
  if (value == null) return false;
  if (Array.isArray(value)) return value.length > 0;
  if (typeof value === "object") return Boolean(value.start) && Boolean(value.end);
  if (typeof value === "number") return true;
  return value !== "";
}

/** Narrows the dashboard param bag to the tokens this widget's SQL uses, only if each has a
 * complete value; else `undefined` so a partial set (half-typed range, unrelated param) never
 * reaches `execute`. */
export function resolveWidgetParams(
  tokens: string[] | undefined,
  dashboardParams: Record<string, ParamValue> | undefined,
): Record<string, ParamValue> | undefined {
  if (!tokens || tokens.length === 0 || !dashboardParams) return undefined;

  const resolved: Record<string, ParamValue> = {};
  for (const token of tokens) {
    const value = dashboardParams[token];
    if (!isCompleteParamValue(value)) return undefined;
    resolved[token] = value;
  }
  return resolved;
}
