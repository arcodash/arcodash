/** Pure helpers for the "Refresh schedule" section: preset<->spec mapping, freshness math, time
 * formatting. */
import { useEffect, useState } from "react";
import type { QueryConsumers, Run, Schedule, ScheduleSpec } from "@/types/api";

export type PresetId =
  | "every_5_min"
  | "every_15_min"
  | "every_30_min"
  | "hourly"
  | "daily"
  | "weekly"
  | "advanced";

export interface PresetForm {
  preset: PresetId;
  /** "HH:MM" for daily/weekly (UTC). */
  time: string;
  day: string;
  cronExpr: string;
}

export const DEFAULT_PRESET_FORM: PresetForm = {
  preset: "every_15_min",
  time: "09:00",
  day: "mon",
  cronExpr: "",
};

const PRESET_INTERVAL_SECS: Partial<Record<PresetId, number>> = {
  every_5_min: 300,
  every_15_min: 900,
  every_30_min: 1800,
  hourly: 3600,
};
const INTERVAL_PRESET = Object.fromEntries(
  Object.entries(PRESET_INTERVAL_SECS).map(([preset, secs]) => [secs, preset]),
) as Record<number, PresetId>;

/** Map a stored spec onto the form. Anything not a preset (arbitrary interval, multi-weekday `at`,
 * `cron`) lands on `advanced`; `cronExpr` is seeded only for `cron`. */
export function specToForm(spec: ScheduleSpec): PresetForm {
  if (spec.kind === "interval") {
    return { ...DEFAULT_PRESET_FORM, preset: INTERVAL_PRESET[spec.every_secs] ?? "advanced" };
  }
  if (spec.kind === "at") {
    const days = spec.days ?? [];
    if (days.length === 0) return { ...DEFAULT_PRESET_FORM, preset: "daily", time: spec.time };
    if (days.length === 1) return { ...DEFAULT_PRESET_FORM, preset: "weekly", time: spec.time, day: days[0] };
    return { ...DEFAULT_PRESET_FORM, preset: "advanced" };
  }
  return { ...DEFAULT_PRESET_FORM, preset: "advanced", cronExpr: spec.expr };
}

/** Build the spec to POST/PATCH. Daily/Weekly use the `at` variant: the backend parses `cron` specs
 * with a 6-field parser, so a hand-built 5-field cron would 400. */
export function formToSpec(form: PresetForm): ScheduleSpec {
  const intervalSecs = PRESET_INTERVAL_SECS[form.preset];
  if (intervalSecs != null) return { kind: "interval", every_secs: intervalSecs };
  if (form.preset === "daily") return { kind: "at", time: form.time };
  if (form.preset === "weekly") return { kind: "at", time: form.time, days: [form.day] };
  return { kind: "cron", expr: form.cronExpr };
}

export const WEEKDAY_LABEL: Record<string, string> = {
  mon: "Monday", tue: "Tuesday", wed: "Wednesday", thu: "Thursday",
  fri: "Friday", sat: "Saturday", sun: "Sunday",
};

export function describeSpec(spec: ScheduleSpec): string {
  if (spec.kind === "interval") {
    const s = spec.every_secs;
    if (s === 300) return "Every 5 min";
    if (s === 900) return "Every 15 min";
    if (s === 1800) return "Every 30 min";
    if (s === 3600) return "Hourly";
    if (s % 3600 === 0) return `Every ${s / 3600} hours`;
    if (s % 60 === 0) return `Every ${s / 60} min`;
    return `Every ${s}s`;
  }
  if (spec.kind === "at") {
    const days = spec.days ?? [];
    if (days.length === 0) return `Daily at ${spec.time} UTC`;
    if (days.length === 1) return `Weekly on ${WEEKDAY_LABEL[days[0]] ?? days[0]} at ${spec.time} UTC`;
    return `${days.map((d) => WEEKDAY_LABEL[d] ?? d).join(", ")} at ${spec.time} UTC`;
  }
  return `Cron: ${spec.expr}`;
}

/** Expected cadence in seconds for staleness; cron/at use `(next_run_at − last_finished_at)` as a proxy.
 * `null` when not derivable, and the caller must then never escalate to amber (a false stale alarm is worse than a missing one). */
export function expectedCadenceSecs(
  spec: ScheduleSpec,
  lastFinishedAtMs: number | null,
  nextRunAtMs: number | null,
): number | null {
  if (spec.kind === "interval") return spec.every_secs;
  if (lastFinishedAtMs == null || nextRunAtMs == null) return null;
  const gap = (nextRunAtMs - lastFinishedAtMs) / 1000;
  return gap > 0 ? gap : null;
}

/** Amber only past 1.5× the expected cadence. `cadenceSecs == null` ⇒ never stale. */
export function isStale(ageSecs: number, cadenceSecs: number | null): boolean {
  return cadenceSecs != null && ageSecs > 1.5 * cadenceSecs;
}

/** Human wording for an expected refresh cadence: `hourly`, `daily`, `weekly`, else `every 15 min`. */
export function cadenceLabel(secs: number): string {
  if (secs === 3600) return "hourly";
  if (secs === 86400) return "daily";
  if (secs === 604800) return "weekly";
  const m = Math.round(secs / 60);
  if (m < 60) return `every ${Math.max(1, m)} min`;
  const h = Math.round(m / 60);
  if (h < 24) return `every ${h}h`;
  return `every ${Math.round(h / 24)}d`;
}

/** Whether a schedule whose last run finished at `finishedMs` is stale at `nowMs`. */
export function isScheduleStale(
  schedule: Pick<Schedule, "spec" | "next_run_at">,
  finishedMs: number,
  nowMs: number,
): boolean {
  const nextMs = schedule.next_run_at ? Date.parse(schedule.next_run_at) : null;
  return isStale((nowMs - finishedMs) / 1000, expectedCadenceSecs(schedule.spec, finishedMs, nextMs));
}

export function timeAgo(fromMs: number, nowMs: number): string {
  const s = Math.max(0, Math.round((nowMs - fromMs) / 1000));
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.round(h / 24)}d ago`;
}

export function countdown(toMs: number, nowMs: number): string {
  const s = Math.round((toMs - nowMs) / 1000);
  if (s <= 0) return "now";
  const m = Math.round(s / 60);
  if (s < 60) return `in ${s}s`;
  if (m < 60) return `in ${m} min`;
  return `in ${Math.round(m / 60)}h`;
}

export function formatDuration(ms: number): string {
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${Math.round(s % 60)}s`;
}

export function elapsed(startMs: number, nowMs: number): string {
  return formatDuration(Math.max(0, nowMs - startMs));
}

export function exactTs(ms: number): string {
  return new Date(ms).toLocaleString();
}

/** A monitor check hit its max-wait ceiling and used stale-fallback data. `maxFreshWaitSecs ==
 * null` ("always wait") never marks it. */
export function isStaleFallback(
  dataAgeSecs: number | null,
  maxFreshWaitSecs: number | null | undefined,
): boolean {
  return maxFreshWaitSecs != null && dataAgeSecs != null && dataAgeSecs > maxFreshWaitSecs;
}

export function impactLine(consumers: QueryConsumers): string | null {
  const parts: string[] = [];
  if (consumers.tile_count > 0) {
    const tiles = `${consumers.tile_count} tile${consumers.tile_count === 1 ? "" : "s"}`;
    const dashboards = `${consumers.dashboard_count} dashboard${consumers.dashboard_count === 1 ? "" : "s"}`;
    parts.push(`${tiles} on ${dashboards}`);
  }
  if (consumers.monitor_count > 0) {
    parts.push(`${consumers.monitor_count} monitor${consumers.monitor_count === 1 ? "" : "s"}`);
  }
  return parts.length > 0 ? `Feeds ${parts.join(" · ")}` : null;
}

export function isInFlight(run: Run): boolean {
  return run.status === "pending" || run.status === "running";
}

/** Failure for chips/dots/expanders, mirroring the worker's `is_failure()`; `cancelled` is excluded
 * (a user stop is not a failure). */
export function isFailedStatus(status: string): boolean {
  return status === "failed" || status === "timed_out";
}

export function isFailedRun(run: Run): boolean {
  return isFailedStatus(run.status);
}

/** True while paused (auto pause or a future `paused_until`), distinct from `!enabled`. Use this for every
 * "is it running" display; checking `enabled`/`auto_paused_at` alone misreads a future `paused_until`. */
export function isSchedulePaused(schedule: Pick<Schedule, "auto_paused_at" | "paused_until"> | null): boolean {
  if (!schedule) return false;
  return (
    schedule.auto_paused_at != null ||
    (schedule.paused_until != null && Date.parse(schedule.paused_until) > Date.now())
  );
}

export function inFlightRun(runs: Run[]): Run | null {
  return runs.find(isInFlight) ?? null;
}

export function runStartMs(run: Run): number | null {
  const t = run.started_at ?? run.scheduled_for;
  return t ? Date.parse(t) : null;
}

/** Re-renders every second while `active`, every 30s otherwise, returning `Date.now()`. Local only (no
 * network). Never stops entirely so a collapsed freshness chip can still go stale. */
export function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), active ? 1000 : 30000);
    return () => clearInterval(id);
  }, [active]);
  return now;
}
