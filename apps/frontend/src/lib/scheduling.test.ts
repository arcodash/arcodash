import { describe, it, expect } from "vitest";
import {
  cadenceLabel,
  countdown,
  describeSpec,
  expectedCadenceSecs,
  formatDuration,
  formToSpec,
  isFailedStatus,
  isScheduleStale,
  isStale,
  isStaleFallback,
  specToForm,
  timeAgo,
  type PresetForm,
} from "@/lib/scheduling";
import type { ScheduleSpec } from "@/types/api";

describe("specToForm / formToSpec round-trip", () => {
  const cases: { label: string; spec: ScheduleSpec; preset: PresetForm["preset"] }[] = [
    { label: "every 5 min", spec: { kind: "interval", every_secs: 300 }, preset: "every_5_min" },
    { label: "every 15 min", spec: { kind: "interval", every_secs: 900 }, preset: "every_15_min" },
    { label: "every 30 min", spec: { kind: "interval", every_secs: 1800 }, preset: "every_30_min" },
    { label: "hourly", spec: { kind: "interval", every_secs: 3600 }, preset: "hourly" },
  ];

  for (const { label, spec, preset } of cases) {
    it(`${label} round-trips`, () => {
      const form = specToForm(spec);
      expect(form.preset).toBe(preset);
      expect(formToSpec(form)).toEqual(spec);
    });
  }

  it("daily round-trips via the `at` spec variant (not a cron string)", () => {
    const spec: ScheduleSpec = { kind: "at", time: "09:00" };
    const form = specToForm(spec);
    expect(form.preset).toBe("daily");
    expect(form.time).toBe("09:00");
    expect(formToSpec(form)).toEqual(spec);
  });

  it("weekly round-trips via the `at` spec variant with a single day", () => {
    const spec: ScheduleSpec = { kind: "at", time: "09:00", days: ["mon"] };
    const form = specToForm(spec);
    expect(form.preset).toBe("weekly");
    expect(form.time).toBe("09:00");
    expect(form.day).toBe("mon");
    expect(formToSpec(form)).toEqual(spec);
  });

  it("advanced (raw cron) round-trips its 6-field expression", () => {
    const spec: ScheduleSpec = { kind: "cron", expr: "0 30 9 * * MON" };
    const form = specToForm(spec);
    expect(form.preset).toBe("advanced");
    expect(form.cronExpr).toBe("0 30 9 * * MON");
    expect(formToSpec(form)).toEqual(spec);
  });

  it("an arbitrary interval not matching a preset lands on advanced", () => {
    const form = specToForm({ kind: "interval", every_secs: 21_600 });
    expect(form.preset).toBe("advanced");
  });

  it("a multi-weekday `at` spec lands on advanced (not representable as a preset)", () => {
    const form = specToForm({ kind: "at", time: "09:00", days: ["mon", "thu"] });
    expect(form.preset).toBe("advanced");
  });
});

describe("describeSpec", () => {
  it.each([
    [{ kind: "interval", every_secs: 300 } as ScheduleSpec, "Every 5 min"],
    [{ kind: "interval", every_secs: 900 } as ScheduleSpec, "Every 15 min"],
    [{ kind: "interval", every_secs: 1800 } as ScheduleSpec, "Every 30 min"],
    [{ kind: "interval", every_secs: 3600 } as ScheduleSpec, "Hourly"],
    [{ kind: "interval", every_secs: 21_600 } as ScheduleSpec, "Every 6 hours"],
    [{ kind: "at", time: "09:00" } as ScheduleSpec, "Daily at 09:00 UTC"],
    [{ kind: "at", time: "09:00", days: ["mon"] } as ScheduleSpec, "Weekly on Monday at 09:00 UTC"],
    [
      { kind: "at", time: "09:00", days: ["mon", "thu"] } as ScheduleSpec,
      "Monday, Thursday at 09:00 UTC",
    ],
    [{ kind: "cron", expr: "0 30 9 * * MON" } as ScheduleSpec, "Cron: 0 30 9 * * MON"],
  ])("%s -> %s", (spec, expected) => {
    expect(describeSpec(spec)).toBe(expected);
  });
});

describe("expectedCadenceSecs", () => {
  it("interval spec returns every_secs regardless of timestamps", () => {
    expect(expectedCadenceSecs({ kind: "interval", every_secs: 900 }, null, null)).toBe(900);
  });

  it("cron spec with both timestamps returns the positive gap", () => {
    const spec: ScheduleSpec = { kind: "cron", expr: "0 0 * * * *" };
    const last = 1_000_000;
    const next = last + 3600_000;
    expect(expectedCadenceSecs(spec, last, next)).toBe(3600);
  });

  it("cron spec with a null timestamp returns null", () => {
    const spec: ScheduleSpec = { kind: "cron", expr: "0 0 * * * *" };
    expect(expectedCadenceSecs(spec, null, 1000)).toBeNull();
    expect(expectedCadenceSecs(spec, 1000, null)).toBeNull();
  });

  it("cron spec where next <= last returns null", () => {
    const spec: ScheduleSpec = { kind: "cron", expr: "0 0 * * * *" };
    expect(expectedCadenceSecs(spec, 2000, 1000)).toBeNull();
    expect(expectedCadenceSecs(spec, 1000, 1000)).toBeNull();
  });
});

describe("isStale", () => {
  it("is false at exactly 1.5x cadence", () => {
    expect(isStale(150, 100)).toBe(false);
  });

  it("is true just past 1.5x cadence", () => {
    expect(isStale(150.1, 100)).toBe(true);
  });

  it("is false when cadence is null, even for a huge age", () => {
    expect(isStale(1_000_000, null)).toBe(false);
  });
});

describe("isScheduleStale", () => {
  it("uses the interval cadence, or next_run_at for cron/at", () => {
    const interval = { spec: { kind: "interval", every_secs: 100 } as ScheduleSpec, next_run_at: null };
    expect(isScheduleStale(interval, 0, 150_000)).toBe(false);
    expect(isScheduleStale(interval, 0, 150_001)).toBe(true);
    const cron = { spec: { kind: "cron", expr: "0 * * * * *" } as ScheduleSpec, next_run_at: new Date(100_000).toISOString() };
    expect(isScheduleStale(cron, 0, 150_001)).toBe(true);
    expect(isScheduleStale({ ...cron, next_run_at: null }, 0, 1e12)).toBe(false);
  });
});

describe("timeAgo", () => {
  it("'just now' for under 45s", () => {
    expect(timeAgo(1000, 1000 + 30_000)).toBe("just now");
  });

  it("'X min ago' for minutes", () => {
    expect(timeAgo(0, 3 * 60_000)).toBe("3 min ago");
  });

  it("'Xh ago' for hours", () => {
    expect(timeAgo(0, 2 * 3600_000)).toBe("2h ago");
  });

  it("'Xd ago' for days", () => {
    expect(timeAgo(0, 3 * 86_400_000)).toBe("3d ago");
  });
});

describe("countdown", () => {
  it("'in Ns' under a minute", () => {
    expect(countdown(40_000, 0)).toBe("in 40s");
  });

  it("'in N min' under an hour", () => {
    expect(countdown(6 * 60_000, 0)).toBe("in 6 min");
  });

  it("'in Nh' for longer spans", () => {
    expect(countdown(2 * 3600_000, 0)).toBe("in 2h");
  });

  it("'now' for a past deadline", () => {
    expect(countdown(-1000, 0)).toBe("now");
    expect(countdown(0, 0)).toBe("now");
  });
});

describe("formatDuration", () => {
  it("sub-minute durations render as seconds with one decimal", () => {
    expect(formatDuration(400)).toBe("0.4s");
    expect(formatDuration(3200)).toBe("3.2s");
  });

  it("durations over a minute render as 'Xm Ys'", () => {
    expect(formatDuration(4 * 60_000 + 10_000)).toBe("4m 10s");
  });
});

describe("isStaleFallback", () => {
  it("true when data age exceeds the ceiling", () => {
    expect(isStaleFallback(700, 600)).toBe(true);
  });

  it("false at or below the ceiling", () => {
    expect(isStaleFallback(600, 600)).toBe(false);
    expect(isStaleFallback(100, 600)).toBe(false);
  });

  it("false when the ceiling is null (always-wait), even for a huge age", () => {
    expect(isStaleFallback(1_000_000, null)).toBe(false);
    expect(isStaleFallback(1_000_000, undefined)).toBe(false);
  });

  it("false when data age is null", () => {
    expect(isStaleFallback(null, 600)).toBe(false);
  });
});

describe("isFailedStatus", () => {
  it("classifies failed and timed_out as failures, cancelled and success as not", () => {
    expect(isFailedStatus("failed")).toBe(true);
    expect(isFailedStatus("timed_out")).toBe(true);
    expect(isFailedStatus("cancelled")).toBe(false);
    expect(isFailedStatus("success")).toBe(false);
  });

  it("does not misclassify an unknown status as a failure", () => {
    expect(isFailedStatus("some_future_status")).toBe(false);
  });
});

describe("cadenceLabel", () => {
  it("names the common cadences", () => {
    expect(cadenceLabel(3600)).toBe("hourly");
    expect(cadenceLabel(86400)).toBe("daily");
    expect(cadenceLabel(604800)).toBe("weekly");
  });

  it("spells out other intervals", () => {
    expect(cadenceLabel(900)).toBe("every 15 min");
    expect(cadenceLabel(7200)).toBe("every 2h");
    expect(cadenceLabel(172800)).toBe("every 2d");
  });
});
