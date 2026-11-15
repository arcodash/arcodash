import type { Dashboard } from "@/types/api";
import type { TileOrigin } from "@/lib/monitorLinks";

const ID_RE = /^[1-9]\d{0,17}$/;
// Slugs are alphanumeric words joined by "-" (plus an optional numeric suffix); never "/" or ".".
const SLUG_RE = /^[\p{L}\p{N}_-]{1,200}$/u;

/** The dashboard tile a new-monitor form was opened from, or null when the params are absent or malformed. */
export function parseTileOrigin(params: URLSearchParams): TileOrigin | null {
  if (params.get("from") !== "tile") return null;
  const queryId = params.get("query_id") ?? "";
  const widget = params.get("widget") ?? "";
  const dashboard = params.get("dashboard") ?? "";
  if (!ID_RE.test(queryId) || !ID_RE.test(widget) || !SLUG_RE.test(dashboard)) return null;
  return { dashboardSlug: dashboard, widgetId: Number(widget) };
}

/** Names for the origin note, only when the loaded dashboard's tile really is for this query. */
export function resolveTileLabel(
  d: Dashboard | undefined,
  origin: TileOrigin,
  queryId: number,
): { tileTitle: string; dashboardName: string } | null {
  const viz = d?.widgets.find((w) => w.id === origin.widgetId)?.visualization;
  if (!d || !viz) return null;
  if ((viz.query?.id ?? viz.query_id) !== queryId) return null;
  return { tileTitle: viz.name, dashboardName: d.name };
}
