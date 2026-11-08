import { describeSpec, expectedCadenceSecs, isSchedulePaused } from "@/lib/scheduling";
import type { Schedule } from "@/types/api";

export interface LearningStatus {
  kind: "active" | "no-eta" | "paused";
  have: number;
  need: number;
  /** Time until the last needed check lands; only for `active`. */
  etaMs?: number;
  alertsAtMs?: number;
  cadenceLabel?: string;
  label: string;
  ariaLabel: string;
}

function formatEta(ms: number): { short: string; long: string } {
  const mins = Math.max(1, Math.round(ms / 60000));
  if (mins < 60) return { short: `~${mins} min`, long: `${mins} ${mins === 1 ? "minute" : "minutes"}` };
  const hours = Math.round(mins / 60);
  if (hours < 48) return { short: `~${hours} h`, long: `${hours} ${hours === 1 ? "hour" : "hours"}` };
  const days = Math.round(hours / 24);
  return { short: `~${days} days`, long: `${days} days` };
}

/** `schedule === undefined` means still loading (treated as no-eta); `null` means none exists. */
export function learningStatus(
  learning: { have: number; need: number },
  schedule: Schedule | null | undefined,
  nowMs: number,
): LearningStatus {
  const { have, need } = learning;
  const progress = `${have} of ${need} checks`;
  const noEta = (): LearningStatus => ({
    kind: "no-eta",
    have,
    need,
    label: `Learning · ${progress}`,
    ariaLabel: `Learning: ${progress}`,
  });
  if (schedule === undefined) return noEta();
  if (!schedule?.spec || !schedule.enabled || isSchedulePaused(schedule)) {
    return {
      kind: "paused",
      have,
      need,
      label: `Learning paused · ${have} of ${need}`,
      ariaLabel: `Learning: ${progress}, learning paused because the check schedule is off`,
    };
  }
  const nextMs = schedule.next_run_at ? Date.parse(schedule.next_run_at) : null;
  const finished = schedule.last_run?.finished_at;
  const cadence = expectedCadenceSecs(schedule.spec, finished ? Date.parse(finished) : null, nextMs);
  const remaining = Math.max(1, need - have);
  if (cadence == null) return noEta();
  const untilNext = nextMs != null ? Math.max(0, nextMs - nowMs) : cadence * 1000;
  const etaMs = untilNext + (remaining - 1) * cadence * 1000;
  const eta = formatEta(etaMs);
  return {
    kind: "active",
    have,
    need,
    etaMs,
    alertsAtMs: nowMs + etaMs,
    cadenceLabel: describeSpec(schedule.spec),
    label: `Learning · alerts in ${eta.short}`,
    ariaLabel: `Learning: ${progress}, alerts in about ${eta.long}`,
  };
}

/** Numbers only observations that carry a value, so slots match the checks counted as learned. */
export function learningSlots<T extends { value: number | null }>(observations: T[]): (T & { slot: number })[] {
  return observations.filter((o) => o.value != null).map((o, slot) => ({ ...o, slot }));
}

/** "14:30" within a day, otherwise "Oct 16". */
export function formatAlertsAt(alertsAtMs: number, nowMs: number): string {
  const d = new Date(alertsAtMs);
  if (alertsAtMs - nowMs > 24 * 3600 * 1000) return d.toLocaleDateString([], { month: "short", day: "numeric" });
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
}
