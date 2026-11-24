export interface TileOrigin {
  dashboardSlug: string;
  widgetId: number;
}

/** New-monitor form link; a tile origin lets the form return to the dashboard it came from. */
export function newMonitorHref(queryId: number, origin?: TileOrigin): string {
  const params = new URLSearchParams({ query_id: String(queryId) });
  if (origin) {
    params.set("from", "tile");
    params.set("dashboard", origin.dashboardSlug);
    params.set("widget", String(origin.widgetId));
  }
  return `/monitors/new?${params.toString()}`;
}
