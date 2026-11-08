/** The collapsible schedule card: toggle/cadence/Run now/impact/banner plus runs table. Query
 * flavor is a `shrink-0` last child so it never eats the SQL editor's flex-1 height. */
import { useEffect, useRef, useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { ChevronDown, ChevronUp, Eye, RefreshCw, type LucideIcon } from "lucide-react";
import { Link } from "react-router";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Loader } from "@/components/ui/loader";
import { Switch } from "@/components/ui/switch";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { DocsLink } from "@/components/common/DocsLink";
import { ApiError, api } from "@/lib/api";
import {
  DEFAULT_PRESET_FORM,
  WEEKDAY_LABEL,
  countdown,
  describeSpec,
  elapsed,
  formToSpec,
  impactLine,
  inFlightRun,
  isFailedRun,
  isInFlight,
  isScheduleStale,
  isSchedulePaused,
  runStartMs,
  specToForm,
  timeAgo,
  useNow,
  type PresetForm,
  type PresetId,
} from "@/lib/scheduling";
import type { Lifecycle } from "@/lib/effectiveState";
import type { Run, Schedule, ScheduleSpec } from "@/types/api";
import { RunsTable } from "./RunsTable";

export interface ScheduleFlavor {
  entityType: "query" | "monitor";
  title: string;
  toggleLabel: string;
  runNowLabel: string;
  runNowPendingLabel: string;
  cadenceLabel: string;
  icon: LucideIcon;
  accent: "neutral" | "amber";
}

export const QUERY_FLAVOR: ScheduleFlavor = {
  entityType: "query",
  title: "Refresh schedule",
  toggleLabel: "Keep this data fresh automatically",
  runNowLabel: "Run now",
  runNowPendingLabel: "Refreshing…",
  cadenceLabel: "Refresh every:",
  icon: RefreshCw,
  accent: "neutral",
};

export const MONITOR_FLAVOR: ScheduleFlavor = {
  entityType: "monitor",
  title: "Check schedule",
  toggleLabel: "Check this condition automatically",
  runNowLabel: "Check now",
  runNowPendingLabel: "Checking…",
  cadenceLabel: "Check every:",
  icon: Eye,
  accent: "amber",
};

/** Monitor-only "Reads `<query>` · refreshed Xm ago" line, using `FreshnessChip`'s stale computation.
 * Amber when the query is stale, never ran or has no schedule (such a monitor watches nothing). */
function ReadsQueryLine({ query }: { query: { id: number; name: string } }) {
  const scheduleQuery = useQuery({
    queryKey: ["schedules", "query", query.id],
    queryFn: () => api.getSchedule("query", query.id),
  });
  const schedule = scheduleQuery.data;
  const finishedMs = schedule?.last_run?.finished_at ? Date.parse(schedule.last_run.finished_at) : null;
  // Date.now() is fine here: the label re-renders on the section's poll cadence.
  const now = Date.now();
  // About the *schedule's* state, not data freshness: a manual run can leave fresh rows with no
  // schedule, so "refreshed never" would misreport.
  const warn = !schedule || finishedMs == null || isScheduleStale(schedule, finishedMs, now);
  return (
    <p className={`text-xs ${warn ? "text-warning-foreground" : "text-muted-foreground"}`}>
      Reads{" "}
      <Link to={`/queries/${query.id}`} className="underline">
        {query.name}
      </Link>{" "}
      ·{" "}
      {!schedule
        ? "no refresh schedule set"
        : finishedMs == null
          ? "scheduled, not run yet"
          : `refreshed ${timeAgo(finishedMs, now)}`}
    </p>
  );
}

const PRESET_OPTIONS: { value: PresetId; label: string }[] = [
  { value: "every_5_min", label: "Every 5 min" },
  { value: "every_15_min", label: "Every 15 min" },
  { value: "every_30_min", label: "Every 30 min" },
  { value: "hourly", label: "Hourly" },
  { value: "daily", label: "Daily at HH:MM" },
  { value: "weekly", label: "Weekly on DAY at HH:MM" },
  // Cron is a separate input mode, reached only via the "Advanced (cron)" disclosure.
];

const WEEKDAYS = Object.keys(WEEKDAY_LABEL);

export interface RunWatch {
  runId: number;
  until: number;
}

const WATCH_WINDOW_MS = 30_000;

function nextFireDueSoon(schedule: Schedule | null | undefined): boolean {
  return !!schedule?.enabled && !!schedule.next_run_at && Date.parse(schedule.next_run_at) - Date.now() <= 15_000;
}

/** QueryEditorPage keys its detail query on the numeric id, MonitorPage on the raw string; invalidate both. */
function invalidateDetail(queryClient: QueryClient, entityType: string, entityId: number) {
  queryClient.invalidateQueries({ queryKey: [entityType, entityId] });
  queryClient.invalidateQueries({ queryKey: [entityType, String(entityId)] });
}

/** Run exists, is terminal, and has no pending notification. A fast run can finish before the first
 * refetch, and the worker records delivery after finalizing, so an empty `notified` may be early. */
function runSettled(runs: Run[] | undefined, runId: number): boolean {
  const run = runs?.find((r) => r.id === runId);
  return (
    !!run &&
    !isInFlight(run) &&
    !(run.condition_met === true && run.notified.length === 0) &&
    run.notified.every((n) => n.status === "sent" || n.status === "failed")
  );
}

/** The only network poll: 3000 while a run is in flight or next fire is imminent, 1500 while a
 * triggered run's watch window is open, else `false` so an idle schedule costs nothing. */
export function schedulePollMs(
  schedule: Schedule | null | undefined,
  runs: Run[] | undefined,
  watch?: RunWatch | null,
): number | false {
  if (runs && inFlightRun(runs)) return 3000;
  if (watch && Date.now() < watch.until && !runSettled(runs, watch.runId)) return 1500;
  return nextFireDueSoon(schedule) ? 3000 : false;
}

/** Same query keys `FreshnessChip` reads (query flavor), so TanStack dedupes the fetch; the monitor
 * flavor uses its own `["schedules","monitor",id]` key. */
function useScheduleData(entityType: "query" | "monitor", entityId: number, watch: RunWatch | null) {
  const queryClient = useQueryClient();
  const pollInterval = () => {
    const schedule = queryClient.getQueryData<Schedule | null>(["schedules", entityType, entityId]);
    const runs = queryClient.getQueryData<Run[]>(["runs", entityType, entityId]);
    return schedulePollMs(schedule, runs, watch);
  };
  const scheduleQuery = useQuery({
    queryKey: ["schedules", entityType, entityId],
    queryFn: () => api.getSchedule(entityType, entityId),
    refetchInterval: pollInterval,
    refetchIntervalInBackground: false,
  });
  const runsQuery = useQuery({
    queryKey: ["runs", entityType, entityId],
    queryFn: () => api.listRuns(entityType, entityId),
    refetchInterval: pollInterval,
    refetchIntervalInBackground: false,
  });
  // `pollInterval` reads another query's lagging cache; a change in the newest run's status is the
  // reliable signal to refresh `last_run`.
  const loaded = runsQuery.data !== undefined;
  const newest = runsQuery.data?.[0];
  const newestKey = newest ? `${newest.id}:${newest.status}` : null;
  const seenKey = useRef<string | null | undefined>(undefined);
  useEffect(() => {
    if (!loaded) return;
    if (seenKey.current !== undefined && newestKey !== seenKey.current) {
      queryClient.invalidateQueries({ queryKey: ["schedules", entityType, entityId] });
    }
    seenKey.current = newestKey;
  }, [loaded, newestKey, queryClient, entityType, entityId]);
  return { scheduleQuery, runsQuery };
}

interface ScheduleSectionProps {
  entityId: number;
  flavor: ScheduleFlavor;
  canEdit: boolean;
  // query-only — the monitor flavor never reads these (no draft/publish split).
  published?: boolean;
  lifecycle?: Lifecycle;
  onPublishClick?: () => void;
  expanded: boolean;
  onExpandedChange: (expanded: boolean) => void;
  /** Expands the section and opens a run's error expander (typically the newest failed). */
  focusRunId: number | null;
  // monitor-only — the query flavor never passes these.
  /** The monitor's underlying query, for the "Reads `<query>`" line. */
  readsQuery?: { id: number; name: string };
  /** Notify/rearm + Max-wait controls built by the page (they save to the monitor, not the
   * schedule). */
  monitorControls?: ReactNode;
  /** The monitor's `max_fresh_wait_secs`; drives the runs table's stale-fallback amber cell. */
  maxFreshWaitSecs?: number | null;
}

export function ScheduleSection({
  entityId,
  flavor,
  canEdit,
  published = true,
  lifecycle = "published_clean",
  onPublishClick,
  expanded,
  onExpandedChange,
  focusRunId,
  readsQuery,
  monitorControls,
  maxFreshWaitSecs,
}: ScheduleSectionProps) {
  const isMonitor = flavor.entityType === "monitor";
  const queryClient = useQueryClient();
  const [watch, setWatch] = useState<RunWatch | null>(null);
  const { scheduleQuery, runsQuery } = useScheduleData(flavor.entityType, entityId, watch);
  const consumersQuery = useQuery({
    queryKey: ["consumers", entityId],
    queryFn: () => api.getQueryConsumers(entityId),
    enabled: !isMonitor,
  });

  const schedule = scheduleQuery.data ?? null;
  const runs = runsQuery.data ?? [];

  const [form, setForm] = useState<PresetForm>(DEFAULT_PRESET_FORM);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [cronError, setCronError] = useState<string | null>(null);
  const [openErrorRunId, setOpenErrorRunId] = useState<number | null>(null);

  // Re-seed when a different schedule arrives. Keyed on id alone: the poll returns a new `spec`
  // each tick and would reset mid-typing.
  // `advancedOpen` is deliberately excluded so a raw-cron spec never auto-opens Advanced.
  useEffect(() => {
    if (schedule) setForm(specToForm(schedule.spec));
    // eslint-disable-next-line react-hooks/exhaustive-deps -- keyed on id only, see above
  }, [schedule?.id]);

  useEffect(() => {
    if (focusRunId != null) {
      setOpenErrorRunId(focusRunId);
      onExpandedChange(true);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focusRunId]);

  const running = inFlightRun(runs);
  // The runs table polls but the entity's detail query (header badge etc.) doesn't, so invalidate
  // it when a run lands.
  const wasRunningRef = useRef(running);
  useEffect(() => {
    if (wasRunningRef.current && !running) invalidateDetail(queryClient, flavor.entityType, entityId);
    wasRunningRef.current = running;
  }, [running, flavor.entityType, entityId, queryClient]);
  // A fast run never produces the running→idle edge; refresh once the watched run settles or the
  // window lapses.
  const watchSettled = watch != null && runSettled(runsQuery.data, watch.runId);
  useEffect(() => {
    if (!watch) return;
    const done = () => {
      invalidateDetail(queryClient, flavor.entityType, entityId);
      setWatch(null);
    };
    if (watchSettled) {
      done();
      return;
    }
    const t = setTimeout(done, Math.max(0, watch.until - Date.now()));
    return () => clearTimeout(t);
  }, [watch, watchSettled, flavor.entityType, entityId, queryClient]);
  const now = useNow(expanded && (!!running || nextFireDueSoon(schedule)));

  function invalidateAfterMutation() {
    queryClient.invalidateQueries({ queryKey: ["schedules"] });
    queryClient.invalidateQueries({ queryKey: ["runs", flavor.entityType, entityId] });
  }

  // ScheduleSection's test mocks @/lib/api without apiErrorMessage, so this stays local.
  function mutationErrorMessage(err: unknown, fallback: string): string {
    return err instanceof ApiError ? err.message : fallback;
  }

  function onSaved() {
    setCronError(null);
    invalidateAfterMutation();
  }

  const createMut = useMutation({
    mutationFn: (spec: ScheduleSpec) =>
      api.createSchedule({ entity_type: flavor.entityType, entity_id: entityId, spec }),
    onSuccess: onSaved,
    onError: (err) => setCronError(mutationErrorMessage(err, "Failed to create schedule")),
  });

  const updateMut = useMutation({
    mutationFn: (body: Parameters<typeof api.updateSchedule>[1]) => {
      if (!schedule) throw new Error("No schedule to update");
      return api.updateSchedule(schedule.id, body);
    },
    onSuccess: onSaved,
    onError: (err) => setCronError(mutationErrorMessage(err, "Failed to update schedule")),
  });

  const deleteMut = useMutation({
    mutationFn: () => {
      if (!schedule) throw new Error("No schedule to remove");
      return api.deleteSchedule(schedule.id);
    },
    onSuccess: invalidateAfterMutation,
    onError: (err) => setCronError(mutationErrorMessage(err, "Failed to remove schedule")),
  });

  const runNowMut = useMutation({
    mutationFn: () => api.runNow(flavor.entityType, entityId),
    onSuccess: (run) => {
      setWatch({ runId: run.id, until: Date.now() + WATCH_WINDOW_MS });
      invalidateAfterMutation();
    },
    onError: (err) => setCronError(mutationErrorMessage(err, "Failed to run now")),
  });

  const isRefreshing = runNowMut.isPending || !!running;
  const isPaused = isSchedulePaused(schedule);

  function handleToggle(checked: boolean) {
    if (schedule) updateMut.mutate({ enabled: checked });
    else if (checked) createMut.mutate(formToSpec(form));
  }

  function commitCadence(next: PresetForm) {
    setForm(next);
    if (!schedule) return;
    if (next.preset === "advanced" && next.cronExpr.trim() === "") return;
    updateMut.mutate({ spec: formToSpec(next) });
  }

  const impact = consumersQuery.data ? impactLine(consumersQuery.data) : null;
  const unpublishedQuery = !isMonitor && !published;

  const collapsedSummary = unpublishedQuery ? (
    <span className="text-muted-foreground">Publish to enable</span>
  ) : isPaused ? (
    <span className="text-warning-foreground">· Paused</span>
  ) : schedule?.enabled ? (
    <span className="text-muted-foreground">
      {describeSpec(schedule.spec)}
      {schedule.next_run_at &&
        ` · ${isMonitor ? "Next check" : "Next run"}: ${countdown(Date.parse(schedule.next_run_at), now)}`}
    </span>
  ) : (
    <span className="text-muted-foreground">Off</span>
  );

  const Icon = flavor.icon;
  const runsLabel = isMonitor ? "Recent checks" : "Recent runs";
  const waitingRun = running && running.waiting_on_run_id != null ? running : null;
  const waitingStartMs = waitingRun ? runStartMs(waitingRun) : null;

  return (
    <div className="shrink-0 rounded-md border border-border bg-card p-3">
      <button
        type="button"
        className="flex w-full cursor-pointer items-center justify-between gap-2 text-sm"
        onClick={() => onExpandedChange(!expanded)}
      >
        <span className="flex items-center gap-2 font-medium">
          <Icon className={`size-4 ${flavor.accent === "amber" ? "text-warning" : ""}`} />
          {flavor.title}
        </span>
        <span className="flex items-center gap-2 text-xs">
          {collapsedSummary}
          {expanded ? <ChevronUp className="size-4" /> : <ChevronDown className="size-4" />}
        </span>
      </button>

      {expanded && (
        <div className="mt-3 space-y-3 border-t border-border pt-3">
          {unpublishedQuery ? (
            <div className="space-y-3">
              <Switch checked={false} disabled />
              <p className="text-sm text-muted-foreground">
                <strong className="text-foreground">Publish this query to schedule refreshes.</strong>
                <br />A schedule always runs the published version — there's nothing to keep fresh
                until you publish.
              </p>
              {canEdit && (
                <Button size="sm" onClick={onPublishClick}>
                  Publish…
                </Button>
              )}
            </div>
          ) : (
            <>
              {isPaused && (
                <div className="space-y-2 rounded-md border border-warning/30 bg-warning/10 px-3 py-2 text-sm text-warning-foreground">
                  <p>
                    <strong>
                      {`⚠ ${isMonitor ? "Checks" : "Refreshes"} paused after ${schedule?.consecutive_failures ?? 0} consecutive failures.`}
                    </strong>
                  </p>
                  <p>
                    {isMonitor
                      ? "The last check couldn't run. This condition won't be evaluated again until you resume."
                      : "The last run failed with an error. Refreshes won't run again until you resume."}
                  </p>
                  <div className="flex items-center gap-3">
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={!canEdit}
                      onClick={() => updateMut.mutate({ enabled: true })}
                    >
                      Resume
                    </Button>
                    <button
                      type="button"
                      className="cursor-pointer underline"
                      onClick={() => {
                        const failed = runs.find(isFailedRun);
                        if (failed) setOpenErrorRunId(failed.id);
                      }}
                    >
                      See the error
                    </button>
                  </div>
                </div>
              )}

              <div className="flex items-center gap-2">
                <Switch
                  checked={schedule?.enabled ?? false}
                  disabled={!canEdit}
                  onCheckedChange={handleToggle}
                />
                <span className="text-sm">{flavor.toggleLabel}</span>
              </div>

              {isMonitor && !schedule?.enabled && (
                <p className="text-xs text-muted-foreground">
                  Checking is off — this condition is not being evaluated at all. (To keep checking
                  but stop notifications, use Mute instead.)
                </p>
              )}

              {isMonitor && readsQuery && <ReadsQueryLine query={readsQuery} />}

              {!isMonitor && lifecycle === "published_pending" && (
                <p className="text-xs text-muted-foreground">
                  Refreshes run the published version, not your unpublished edits.{" "}
                  <DocsLink topic="scheduledRefresh" label="scheduled refresh" />
                </p>
              )}

              <div className="flex flex-wrap items-end gap-2">
                <span className="text-xs text-muted-foreground">{flavor.cadenceLabel}</span>
                <Select
                  value={form.preset}
                  onValueChange={(v) => commitCadence({ ...form, preset: v as PresetId })}
                  disabled={!canEdit}
                >
                  <SelectTrigger className="w-56" aria-label={flavor.cadenceLabel}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {PRESET_OPTIONS.map((o) => (
                      <SelectItem key={o.value} value={o.value}>
                        {o.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>

                {(form.preset === "daily" || form.preset === "weekly") && (
                  <div className="flex items-end gap-2">
                    <label className="space-y-1 text-xs text-muted-foreground">
                      Time (UTC)
                      <Input
                        type="time"
                        value={form.time}
                        disabled={!canEdit}
                        onChange={(e) => setForm({ ...form, time: e.target.value })}
                        onBlur={() => commitCadence(form)}
                      />
                    </label>
                    {form.preset === "weekly" && (
                      <Select
                        value={form.day}
                        onValueChange={(v) => commitCadence({ ...form, day: v })}
                        disabled={!canEdit}
                      >
                        <SelectTrigger className="w-32">
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          {WEEKDAYS.map((d) => (
                            <SelectItem key={d} value={d}>
                              {WEEKDAY_LABEL[d]}
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                    )}
                  </div>
                )}
              </div>

              <div>
                <button
                  type="button"
                  className="cursor-pointer text-xs text-muted-foreground underline"
                  onClick={() => setAdvancedOpen((v) => !v)}
                >
                  ▸ Advanced (cron)
                </button>
                {advancedOpen ? (
                  <div className="mt-1.5 space-y-1">
                    <Input
                      value={form.cronExpr}
                      placeholder="0 */15 * * * *"
                      disabled={!canEdit}
                      onChange={(e) => setForm({ ...form, cronExpr: e.target.value, preset: "advanced" })}
                      onBlur={() => commitCadence({ ...form, preset: "advanced" })}
                    />
                  </div>
                ) : (
                  form.preset === "advanced" &&
                  schedule && (
                    <p className="mt-1 text-xs text-muted-foreground">
                      Custom cadence: {describeSpec(schedule.spec)}
                    </p>
                  )
                )}
                {cronError && <p className="mt-1 text-xs text-destructive">{cronError}</p>}
                {schedule && (
                  <button
                    type="button"
                    className="mt-2 block cursor-pointer text-xs text-destructive underline disabled:cursor-not-allowed disabled:text-muted-foreground disabled:no-underline"
                    disabled={!canEdit}
                    onClick={() => deleteMut.mutate()}
                  >
                    Remove schedule
                  </button>
                )}
              </div>

              {isMonitor && monitorControls && (
                <div className="space-y-3 border-t border-border pt-3">{monitorControls}</div>
              )}

              <div className="flex items-center gap-3">
                <span className="text-xs text-muted-foreground">
                  {isMonitor ? "Next check:" : "Next run:"}{" "}
                  {isPaused
                    ? "paused"
                    : schedule?.enabled && schedule.next_run_at
                      ? countdown(Date.parse(schedule.next_run_at), now)
                      : "—"}
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  disabled={!canEdit || isRefreshing}
                  onClick={() => runNowMut.mutate()}
                >
                  {isRefreshing ? flavor.runNowPendingLabel : flavor.runNowLabel}
                </Button>
              </div>

              {isMonitor && waitingRun && (
                <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                  <Loader size={14} />
                  <span>
                    <strong>Waiting for fresh data…</strong> Your data is being refreshed right now
                    {waitingStartMs != null && ` (started ${elapsed(waitingStartMs, now)} ago)`}
                    . We'll check the condition as soon as it finishes so this alert reflects the
                    latest numbers.
                  </span>
                </div>
              )}

              {!isMonitor && lifecycle === "published_pending" && (
                <p className="text-xs text-muted-foreground">Runs the published version.</p>
              )}

              {!isMonitor && impact && (
                <p className="text-xs text-muted-foreground">{impact}</p>
              )}

              <div>
                <p className="border-b border-border pb-1.5 text-xs font-medium text-muted-foreground">
                  {runsLabel}
                </p>
                <div className="max-h-80 overflow-auto">
                  <RunsTable
                    entityType={flavor.entityType}
                    runs={runs}
                    now={now}
                    openErrorRunId={openErrorRunId}
                    onToggleError={setOpenErrorRunId}
                    onRetry={() => runNowMut.mutate()}
                    canRetry={canEdit}
                    maxFreshWaitSecs={maxFreshWaitSecs}
                  />
                </div>
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
