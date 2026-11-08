import type { PublicWidget, ResultData, Widget } from "@/types/api";

/** Adapts the stripped public/embed shape (`PublicWidget`) into the `Widget[]` that
 * `DashboardGrid`/`WidgetCard` expect, so both public pages share one rendering path. */
export function toGridWidget(pw: PublicWidget): Widget {
  return {
    id: pw.id,
    dashboard_id: 0,
    visualization_id: pw.visualization ? pw.id : null,
    visualization: pw.visualization
      ? {
          id: pw.id,
          query_id: 0,
          type: pw.visualization.type,
          name: pw.visualization.name,
          options: pw.visualization.options,
          created_at: "",
          updated_at: "",
        }
      : undefined,
    query_result_data: (pw.visualization?.data as ResultData | null | undefined) ?? null,
    freshness: pw.freshness ?? null,
    monitors: pw.monitors ?? null,
    // A public/embed link grants the whole cached snapshot regardless of the viewer's ACL, so
    // there's no restricted state.
    viewer_can_read_data: true,
    text: pw.text,
    width: pw.width,
    options: {
      ...(pw.position ? { position: pw.position } : {}),
      // The returned tiles are the full set, so the id list is just their ids.
      ...(pw.monitors && pw.monitors.length > 0 ? { monitors: pw.monitors.map((m) => m.id) } : {}),
    },
    created_at: "",
    updated_at: "",
  };
}
