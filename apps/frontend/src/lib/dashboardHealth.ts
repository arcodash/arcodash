import { isFailedStatus, timeAgo } from "@/lib/scheduling";
import type { Widget } from "@/types/api";

export interface DashboardHealth {
  oldestRetrievedMs: number | null;
  failing: number;
  notRun: number;
  passing: number;
  anyFailedRun: boolean;
}

/** Rolls up the viz tiles that carry freshness; counts are per tile, so tiles sharing a query count twice. */
export function summarizeDashboardHealth(widgets: Widget[]): DashboardHealth {
  const h: DashboardHealth = { oldestRetrievedMs: null, failing: 0, notRun: 0, passing: 0, anyFailedRun: false };
  for (const w of widgets) {
    if (w.visualization_id == null || !w.freshness) continue;
    const ms = Date.parse(w.freshness.retrieved_at);
    if (Number.isNaN(ms)) continue;
    h.oldestRetrievedMs = h.oldestRetrievedMs == null ? ms : Math.min(h.oldestRetrievedMs, ms);
    if (isFailedStatus(w.freshness.last_run_status)) h.anyFailedRun = true;
    h.failing += w.check_summary?.triggered ?? 0;
    h.notRun += w.check_summary?.unevaluable ?? 0;
    h.passing += w.check_summary?.ok ?? 0;
  }
  return h;
}

export interface HealthSegment {
  text: string;
  failing?: boolean;
}

export function healthSegments(h: DashboardHealth, nowMs: number): HealthSegment[] | null {
  if (h.oldestRetrievedMs == null) return null;
  const segs: HealthSegment[] = [{ text: `Oldest result ${timeAgo(h.oldestRetrievedMs, nowMs)}` }];
  if (h.failing > 0) segs.push({ text: `${h.failing} ${h.failing === 1 ? "check" : "checks"} failing`, failing: true });
  if (h.notRun > 0) segs.push({ text: `${h.notRun} not run` });
  if (h.passing > 0) segs.push({ text: `${h.passing} passing` });
  return segs;
}

export function healthLine(h: DashboardHealth, nowMs: number): string | null {
  return healthSegments(h, nowMs)?.map((s) => s.text).join(" · ") ?? null;
}
