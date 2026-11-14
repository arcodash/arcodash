import { Fragment, useMemo } from "react";
import { healthSegments, summarizeDashboardHealth } from "@/lib/dashboardHealth";
import { useNow } from "@/lib/scheduling";
import type { Widget } from "@/types/api";

/** One-line rollup of result age and check state across the dashboard's tiles. */
export function DashboardHealthLine({ widgets }: { widgets: Widget[] }) {
  const now = useNow(false);
  const health = useMemo(() => summarizeDashboardHealth(widgets), [widgets]);
  const segments = healthSegments(health, now);
  if (!segments) return null;
  return (
    <p className="text-xs text-muted-foreground" data-testid="dashboard-health-line">
      {segments.map((s, i) => (
        <Fragment key={s.text}>
          {i > 0 && " · "}
          <span className={s.failing ? "text-destructive" : undefined}>{s.text}</span>
        </Fragment>
      ))}
    </p>
  );
}
