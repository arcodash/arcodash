import ReactGridLayout, { useContainerWidth } from "react-grid-layout";
import "react-grid-layout/css/styles.css";
import { useCallback, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { api, apiErrorMessage } from "@/lib/api";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { WidgetCard, isMonitorWidget } from "./WidgetCard";
import type { Widget, WidgetPosition, ParamValue } from "@/types/api";
import type { LayoutItem, Layout } from "react-grid-layout";

interface Props {
  widgets: Widget[];
  editMode: boolean;
  onDeleteWidget: (id: number) => void;
  dashboardParams?: Record<string, ParamValue>;
  refreshKey?: number;
  /** Required to save layout drags — omitted in read-only render paths that never drag. */
  dashboardSlug?: string;
  publicView?: boolean;
  highlightWidgetId?: number | null;
  /** Query Results data source id, enabling "Explore this tile" on private dashboards. */
  exploreSourceId?: number | null;
}

export function toLayout(widgets: Widget[]): LayoutItem[] {
  return widgets.map((w) => {
    const p = w.options?.position;
    // Monitor cards need two rows so a wrapped title never pushes the status out of view.
    const minH = isMonitorWidget(w) ? 2 : undefined;
    return {
      i: String(w.id),
      x: p?.col ?? 0,
      y: p?.row ?? 0,
      w: p?.sizeX ?? 3,
      h: Math.max(p?.sizeY ?? 2, minH ?? 0),
      minH,
    };
  });
}

export function DashboardGrid({
  widgets,
  editMode,
  onDeleteWidget,
  dashboardParams,
  refreshKey,
  dashboardSlug,
  publicView,
  highlightWidgetId,
  exploreSourceId,
}: Props) {
  const queryClient = useQueryClient();
  const { width, mounted, containerRef } = useContainerWidth();
  const isPhone = !useMediaQuery("(min-width: 768px)");
  const [layoutError, setLayoutError] = useState<string | null>(null);

  // Bounded drag clamps to the container edge ignoring containerPadding, so any padding would let a
  // tile overshoot.
  const gridConfig = useMemo(() => ({ cols: 6, rowHeight: 100, containerPadding: [0, 0] as const }), []);
  // Each save carries the full layout, so an older PUT landing last would clobber a newer one.
  const saveChain = useRef<Promise<unknown>>(Promise.resolve());
  const dragConfig = useMemo(
    () => ({
      enabled: editMode,
      bounded: true,
      handle: ".tile-drag-handle",
      cancel: "button, a, input, select, textarea",
    }),
    [editMode],
  );
  const resizeConfig = useMemo(() => ({ enabled: editMode }), [editMode]);

  // Save on gesture end, not onLayoutChange, so the grid's mount-time compaction doesn't write a
  // draft.
  const handleGestureStop = useCallback(
    (layout: Layout) => {
      if (!editMode || !dashboardSlug) return;
      // One request carries every widget's position, so a multi-tile drag is a single
      // all-or-nothing write.
      const prevById = new Map(widgets.map((w) => [w.id, w.options?.position]));
      const entries = layout
        .filter((item) => prevById.has(Number(item.i)))
        .map((item) => ({
          widget_id: Number(item.i),
          position: { col: item.x, row: item.y, sizeX: item.w, sizeY: item.h } satisfies WidgetPosition,
        }));

      const changed = entries.some(({ widget_id, position }) => {
        const prev = prevById.get(widget_id);
        return (["col", "row", "sizeX", "sizeY"] as const).some((k) => prev?.[k] !== position[k]);
      });
      if (!changed) return;

      setLayoutError(null);
      saveChain.current = saveChain.current
        .then(() => api.saveDashboardLayout(dashboardSlug, entries))
        .then(() => queryClient.invalidateQueries({ queryKey: ["dashboard", dashboardSlug] }))
        .catch((err) => setLayoutError(apiErrorMessage(err, "Failed to save layout")));
    },
    [editMode, widgets, dashboardSlug, queryClient],
  );

  const card = (w: Widget, draggable?: boolean) => (
    <WidgetCard
      widget={w}
      editMode={editMode}
      draggable={draggable}
      onDelete={() => onDeleteWidget(w.id)}
      dashboardParams={dashboardParams}
      refreshKey={refreshKey}
      publicView={publicView}
      highlight={w.id === highlightWidgetId}
      exploreSourceId={exploreSourceId}
    />
  );
  const errorBanner = layoutError && <p className="text-sm text-destructive">{layoutError}</p>;

  if (isPhone) {
    const sorted = [...widgets].sort((a, b) => {
      const pa = a.options?.position;
      const pb = b.options?.position;
      return (pa?.row ?? 0) - (pb?.row ?? 0) || (pa?.col ?? 0) - (pb?.col ?? 0);
    });
    return (
      <>
        {errorBanner}
        <div className="flex flex-col gap-4" data-testid="dashboard-stack">
          {sorted.map((w) => {
            // `min-height` alone gives a percentage-height descendant (chart `ResponsiveContainer`)
            // no box to resolve against; a concrete height scaled like the desktop rowHeight fixes
            // it (200px floor).
            const sizeY = w.options?.position?.sizeY ?? 2;
            const height = Math.max(sizeY * 100, 200);
            return (
              <div key={String(w.id)} className="w-full" style={{ height }}>
                {card(w, false)}
              </div>
            );
          })}
        </div>
      </>
    );
  }

  return (
    <>
      {errorBanner}
      <div ref={containerRef}>
        {mounted && (
          <ReactGridLayout
            layout={toLayout(widgets)}
            gridConfig={gridConfig}
            dragConfig={dragConfig}
            resizeConfig={resizeConfig}
            width={width}
            onDragStop={handleGestureStop}
            onResizeStop={handleGestureStop}
            className="layout"
          >
            {widgets.map((w) => (
              <div key={String(w.id)}>{card(w)}</div>
            ))}
          </ReactGridLayout>
        )}
      </div>
    </>
  );
}
