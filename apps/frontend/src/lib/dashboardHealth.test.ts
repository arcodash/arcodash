import { describe, expect, it } from "vitest";
import { healthLine, summarizeDashboardHealth } from "@/lib/dashboardHealth";
import type { CheckSummary, Widget } from "@/types/api";

const NOW = Date.parse("2026-01-01T12:00:00Z");
const ago = (ms: number) => new Date(NOW - ms).toISOString();
const MIN = 60_000;

function viz(retrievedAgoMs: number | null, summary: CheckSummary | null, status = "success"): Widget {
  return {
    id: 1,
    visualization_id: 10,
    freshness:
      retrievedAgoMs == null
        ? null
        : { retrieved_at: ago(retrievedAgoMs), expected_interval_secs: null, last_run_status: status },
    check_summary: summary,
  } as unknown as Widget;
}

const sum = (ok: number, triggered: number, unevaluable: number): CheckSummary => ({
  total: ok + triggered + unevaluable,
  ok,
  triggered,
  unevaluable,
});

describe("healthLine", () => {
  it("summarizes oldest age and check counts", () => {
    const widgets = [
      viz(12 * MIN, sum(1, 0, 0)),
      viz(120 * MIN, sum(2, 1, 0)),
      viz(5 * MIN, sum(0, 0, 1)),
    ];
    const h = summarizeDashboardHealth(widgets);
    expect(healthLine(h, NOW)).toBe("Oldest result 2h ago · 1 check failing · 1 not run · 3 passing");
  });

  it("returns null with no viz widgets", () => {
    expect(healthLine(summarizeDashboardHealth([]), NOW)).toBeNull();
  });

  it("omits zero-count segments", () => {
    const h = summarizeDashboardHealth([viz(5 * MIN, sum(0, 0, 0))]);
    expect(healthLine(h, NOW)).toBe("Oldest result 5 min ago");
  });

  it("ignores text widgets and viz widgets without freshness", () => {
    const text = { id: 2, visualization_id: null, freshness: null, check_summary: sum(0, 5, 0) } as unknown as Widget;
    const h = summarizeDashboardHealth([text, viz(null, sum(0, 3, 0)), viz(5 * MIN, null)]);
    expect(h.failing).toBe(0);
    expect(healthLine(h, NOW)).toBe("Oldest result 5 min ago");
  });

  it("pluralizes failing checks", () => {
    const h = summarizeDashboardHealth([viz(5 * MIN, sum(0, 2, 0))]);
    expect(healthLine(h, NOW)).toBe("Oldest result 5 min ago · 2 checks failing");
  });

  it("flags failed runs and skips unparsable timestamps", () => {
    const bad = viz(5 * MIN, null);
    bad.freshness!.retrieved_at = "nope";
    const h = summarizeDashboardHealth([bad, viz(5 * MIN, null, "failed")]);
    expect(h.anyFailedRun).toBe(true);
    expect(h.oldestRetrievedMs).toBe(NOW - 5 * MIN);
  });
});
