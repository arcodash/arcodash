/** Freshness signal in the query header, visible without expanding the schedule section. */
import { useQuery } from "@tanstack/react-query";
import { Badge } from "@/components/ui/badge";
import { Loader } from "@/components/ui/loader";
import { api } from "@/lib/api";
import {
  elapsed,
  exactTs,
  inFlightRun,
  isFailedStatus,
  isInFlight,
  isScheduleStale,
  runStartMs,
  timeAgo,
  useNow,
} from "@/lib/scheduling";
import type { Lifecycle } from "@/lib/effectiveState";

interface FreshnessChipProps {
  queryId: number;
  lifecycle: Lifecycle;
  /** Expand the section and focus the newest failed run's error expander; `runId` is null if it
   * cannot be resolved. */
  onOpenFailed: (runId: number | null) => void;
}

export function FreshnessChip({ queryId, lifecycle, onOpenFailed }: FreshnessChipProps) {
  const scheduleQuery = useQuery({
    queryKey: ["schedules", "query", queryId],
    queryFn: () => api.getSchedule("query", queryId),
  });
  const runsQuery = useQuery({
    queryKey: ["runs", "query", queryId],
    queryFn: () => api.listRuns("query", queryId),
  });

  const runs = runsQuery.data ?? [];
  const running = inFlightRun(runs);
  const now = useNow(!!running);

  const schedule = scheduleQuery.data;
  if (!schedule) return null;

  if (running) {
    const startMs = runStartMs(running);
    return (
      <Badge variant="outline" className="gap-1.5 text-muted-foreground" data-testid="freshness-chip">
        <Loader size={14} />
        Refreshing… {startMs != null && `(${elapsed(startMs, now)})`}
      </Badge>
    );
  }

  const lastRun = schedule.last_run;
  if (!lastRun) return null;

  const finishedMs = lastRun.finished_at ? Date.parse(lastRun.finished_at) : null;
  if (isFailedStatus(lastRun.status)) {
    // The newest non-in-flight run is `schedule.last_run` (which has no id); resolve it to focus
    // that row.
    const failedRun = runs.find((r) => !isInFlight(r));
    return (
      <button
        type="button"
        onClick={() => onOpenFailed(failedRun?.id ?? null)}
        className="cursor-pointer"
      >
        <Badge
          variant="outline"
          className="gap-1.5 border-destructive/30 bg-destructive/10 text-destructive"
          title={finishedMs != null ? exactTs(finishedMs) : undefined}
          data-testid="freshness-chip"
        >
          Last refresh failed {finishedMs != null ? timeAgo(finishedMs, now) : ""} ⓘ
        </Badge>
      </button>
    );
  }

  if (finishedMs == null) return null;

  const stale = isScheduleStale(schedule, finishedMs, now);

  const agoText = timeAgo(finishedMs, now);
  const text = `Updated ${agoText}`;
  const title = lifecycle === "published_pending" ? `Published data — updated ${agoText}` : text;

  return (
    <Badge
      variant="outline"
      className={
        stale
          ? "gap-1.5 border-warning/30 bg-warning/10 text-warning-foreground"
          : "gap-1.5 text-muted-foreground"
      }
      title={title}
      aria-label={title}
      data-testid="freshness-chip"
    >
      {text}
    </Badge>
  );
}
