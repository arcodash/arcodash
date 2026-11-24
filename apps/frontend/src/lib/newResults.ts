import { useEffect } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { resolveWidgetParams } from "@/lib/widgetParams";
import type { Dashboard, ParamValue, Widget } from "@/types/api";

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

/** Polling applies only to the private view of a saved dashboard with no auto-refresh set. */
export function newResultsEnabled(o: {
  slug: string | undefined;
  editMode: boolean;
  refreshInterval: number;
}): boolean {
  return o.slug != null && !o.editMode && o.refreshInterval === 0;
}

/** Polls the published dashboard for newer results, starting once the visible dashboard has
 * loaded (its payload seeds the poll cache so the first poll fires a full interval later). The key
 * sits outside `["dashboard", slug]` so mutation invalidations never touch it.
 * `initialData` would not apply: the disabled query is built before the seed exists. */
export function useLatestWidgets(slug: string | undefined, enabled: boolean): Widget[] | undefined {
  const qc = useQueryClient();
  const seed = qc.getQueryData<Dashboard>(["dashboard", slug, false]);
  useEffect(() => {
    const k = ["dashboard-latest", slug];
    if (enabled && seed && !qc.getQueryData(k)) qc.setQueryData(k, seed);
  }, [enabled, seed, qc, slug]);
  const q = useQuery({
    queryKey: ["dashboard-latest", slug],
    queryFn: () => api.getDashboard(slug!, { draft: false }),
    enabled: enabled && seed != null,
    refetchInterval: NEW_RESULTS_POLL_MS,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: true,
    staleTime: NEW_RESULTS_POLL_MS - 5_000,
  });
  return q.data?.widgets;
}
