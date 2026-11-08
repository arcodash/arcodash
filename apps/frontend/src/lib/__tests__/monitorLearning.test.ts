import { describe, it, expect } from "vitest";
import { formatAlertsAt, learningSlots, learningStatus } from "../monitorLearning";
import type { Schedule, ScheduleSpec } from "@/types/api";

const NOW = Date.parse("2026-01-01T12:00:00Z");
const min = (m: number) => new Date(NOW + m * 60000).toISOString();

function sched(spec: ScheduleSpec, over: Partial<Schedule> = {}): Schedule {
  return {
    id: 1,
    entity_type: "monitor",
    entity_id: 1,
    spec,
    timezone: "UTC",
    enabled: true,
    paused_until: null,
    auto_paused_at: null,
    overlap_policy: "skip",
    next_run_at: min(10),
    consecutive_failures: 0,
    last_run: null,
    ...over,
  };
}
const hourly: ScheduleSpec = { kind: "interval", every_secs: 3600 };

describe("learningStatus", () => {
  it("is paused with no, disabled or auto-paused schedule", () => {
    for (const s of [null, sched(hourly, { enabled: false }), sched(hourly, { auto_paused_at: min(-5) })]) {
      const st = learningStatus({ have: 1, need: 7 }, s, NOW);
      expect(st.kind).toBe("paused");
      expect(st.label).toBe("Learning paused · 1 of 7");
      expect(st.ariaLabel).toContain("schedule is off");
    }
  });

  it("has no eta while the schedule is loading or the cadence is not derivable", () => {
    expect(learningStatus({ have: 2, need: 7 }, undefined, NOW).label).toBe("Learning · 2 of 7 checks");
    const cron = sched({ kind: "cron", expr: "0 0 9 * * 1-5" });
    expect(learningStatus({ have: 2, need: 7 }, cron, NOW).kind).toBe("no-eta");
  });

  it("measures hourly eta from next_run_at", () => {
    const st = learningStatus({ have: 1, need: 7 }, sched(hourly), NOW);
    expect(st.kind).toBe("active");
    expect(st.etaMs).toBe(10 * 60000 + 5 * 3600000);
    expect(st.label).toBe("Learning · alerts in ~5 h");
    expect(st.ariaLabel).toBe("Learning: 1 of 7 checks, alerts in about 5 hours");
  });

  it("formats minutes for a 15-min cadence and days for a daily one", () => {
    const quarter = sched({ kind: "interval", every_secs: 900 }, { next_run_at: min(5) });
    expect(learningStatus({ have: 6, need: 7 }, quarter, NOW).label).toBe("Learning · alerts in ~5 min");
    const daily = sched({ kind: "at", time: "09:00" }, { next_run_at: min(60), last_run: { status: "succeeded", finished_at: min(-1380), duration_ms: 1, error: null } });
    expect(learningStatus({ have: 0, need: 7 }, daily, NOW).label).toBe("Learning · alerts in ~6 days");
  });

  it("clamps eta to the minimum when next_run_at is already past", () => {
    const st = learningStatus({ have: 6, need: 7 }, sched(hourly, { next_run_at: min(-30) }), NOW);
    expect(st.etaMs).toBe(0);
    expect(st.label).toBe("Learning · alerts in ~1 min");
  });

  it("is paused while paused_until is in the future", () => {
    const st = learningStatus({ have: 2, need: 7 }, sched(hourly, { paused_until: new Date(Date.now() + 3600000).toISOString() }), NOW);
    expect(st.kind).toBe("paused");
  });
});

describe("formatAlertsAt", () => {
  it("uses a date form beyond 24h and a clock time within a day", () => {
    expect(formatAlertsAt(NOW + 3 * 24 * 3600000, NOW)).not.toContain(":");
    expect(formatAlertsAt(NOW + 3600000, NOW)).toMatch(/\d{2}:\d{2}/);
  });
});

describe("learningSlots", () => {
  it("skips null-valued observations when numbering slots", () => {
    const slots = learningSlots([{ value: 1 }, { value: null }, { value: 3 }]);
    expect(slots.map((o) => o.slot)).toEqual([0, 1]);
    expect(slots.map((o) => o.value)).toEqual([1, 3]);
  });
});
