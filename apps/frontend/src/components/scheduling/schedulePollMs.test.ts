import { describe, expect, it } from "vitest";
import type { Run } from "@/types/api";
import { schedulePollMs } from "./ScheduleSection";

const run = (over: Partial<Run>): Run => ({
  id: 1,
  status: "success",
  triggered_by: "manual",
  scheduled_for: null,
  started_at: null,
  finished_at: null,
  duration_ms: null,
  row_count: null,
  error: null,
  data_age_secs: null,
  waiting_on_run_id: null,
  query_result_id: null,
  condition_met: null,
  notified: [],
  ...over,
});

const watch = () => ({ runId: 1, until: Date.now() + 10_000 });

describe("schedulePollMs watch window", () => {
  it("keeps polling an alerting run whose delivery row has not landed yet", () => {
    expect(schedulePollMs(null, [run({ condition_met: true })], watch())).toBe(1500);
  });

  it("stops once the delivery is sent", () => {
    const notified = [{ destination_name: "Email", status: "sent", error: null }];
    expect(schedulePollMs(null, [run({ condition_met: true, notified })], watch())).toBe(false);
  });

  it("stops for a non-alerting finished run", () => {
    expect(schedulePollMs(null, [run({ condition_met: false })], watch())).toBe(false);
  });
});
