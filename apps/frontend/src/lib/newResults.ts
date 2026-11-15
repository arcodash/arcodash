import { resolveWidgetParams } from "@/components/dashboard/WidgetCard";
import type { ParamValue, Widget } from "@/types/api";

export const NEW_RESULTS_POLL_MS = 60_000;

/** Viz tiles on the latest-result path whose polled `retrieved_at` is strictly newer than the
 * result they render. Tiles that never reported a render, can't be read, or run with dashboard
 * params are skipped, since a re-poll would not change what they show. */
export function staleTileIds(
  latest: Widget[],
  shown: Readonly<Record<number, string>>,
  dashboardParams: Record<string, ParamValue>,
): number[] {
  const ids: number[] = [];
  for (const w of latest) {
    if (w.visualization_id == null || !w.viewer_can_read_data) continue;
    if (resolveWidgetParams(w.visualization?.query?.parameter_names, dashboardParams) !== undefined) continue;
    const shownAt = shown[w.id];
    const polledAt = w.freshness?.retrieved_at;
    if (shownAt == null || polledAt == null) continue;
    const shownMs = Date.parse(shownAt);
    const polledMs = Date.parse(polledAt);
    if (Number.isNaN(shownMs) || Number.isNaN(polledMs)) continue;
    if (polledMs > shownMs) ids.push(w.id);
  }
  return ids;
}

export function newResultsLabel(count: number): string {
  return `New results available for ${count} ${count === 1 ? "tile" : "tiles"}`;
}
