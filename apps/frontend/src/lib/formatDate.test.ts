import { afterEach, describe, it, expect, vi } from "vitest";
import { computeDateGranularity, formatDateValue } from "./formatDate";

describe("computeDateGranularity", () => {
  it("detects month granularity when every value is UTC-midnight on the 1st", () => {
    const values = ["2022-07-01T00:00:00Z", "2022-08-01T00:00:00Z", "2022-09-01T00:00:00Z"];
    expect(computeDateGranularity(values)).toBe("month");
  });

  it("detects day granularity when every value is UTC-midnight but not all on the 1st", () => {
    const values = ["2026-07-01T00:00:00Z", "2026-07-05T00:00:00Z"];
    expect(computeDateGranularity(values)).toBe("day");
  });

  it("falls back to datetime granularity when any value has a time component", () => {
    const values = ["2026-07-05T00:00:00Z", "2026-07-05T15:30:00Z"];
    expect(computeDateGranularity(values)).toBe("datetime");
  });

  it("falls back to datetime granularity for an empty or all-unparseable value set", () => {
    expect(computeDateGranularity([])).toBe("datetime");
    expect(computeDateGranularity(["not a date", null, undefined])).toBe("datetime");
  });
});

describe("formatDateValue", () => {
  it("formats month granularity as e.g. 'Jul 2022'", () => {
    expect(formatDateValue("2022-07-01T00:00:00Z", "month")).toBe("Jul 2022");
  });

  it("formats day granularity as e.g. 'Jul 5, 2026'", () => {
    expect(formatDateValue("2026-07-05T00:00:00Z", "day")).toBe("Jul 5, 2026");
  });

  describe("under a negative-offset local timezone", () => {
    const originalTZ = process.env.TZ;

    afterEach(() => {
      process.env.TZ = originalTZ;
      vi.resetModules();
    });

    // The formatters are built once at module load from `new Intl.DateTimeFormat`, so merely
    // setting process.env.TZ does nothing to an already-imported module — reload it under the
    // new TZ so this actually exercises the UTC pin instead of passing by accident.
    it("renders a UTC-midnight day/month value on the same calendar day, not shifted to the previous day", async () => {
      process.env.TZ = "America/New_York";
      vi.resetModules();
      const fresh = await import("./formatDate");
      const utcMidnight = "2026-07-05T00:00:00Z";
      expect(fresh.formatDateValue(utcMidnight, "day")).toBe("Jul 5, 2026");
      expect(fresh.formatDateValue(utcMidnight, "month")).toBe("Jul 2026");
      expect(fresh.formatDateValue("2022-07-01T00:00:00Z", "month")).toBe("Jul 2022");
    });
  });

  it("formats datetime granularity with both date and time", () => {
    const result = formatDateValue("2026-07-05T15:30:00Z", "datetime");
    expect(result).toContain("2026");
    expect(result).toMatch(/\d{1,2}:\d{2}/);
  });

  it("passes through an unparseable value unchanged rather than throwing", () => {
    expect(formatDateValue("not a date", "day")).toBe("not a date");
    expect(formatDateValue(null, "day")).toBe("");
  });
});
