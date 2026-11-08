import { afterEach, describe, it, expect, vi } from "vitest";
import { api, ApiError } from "@/lib/api";
import {
  OPS,
  OP_LABELS,
  opLabel,
  isComparableColumn,
  formatComparisonValue,
  createMonitorFromSql,
  createMonitorOnQuery,
  PartialCheckError,
  suggestionOptions,
} from "./monitors";
import type { QueryMonitorSuggestion } from "@/types/api";

describe("OP_LABELS", () => {
  it("has a plain-language phrase and a symbol for every operator", () => {
    for (const op of OPS) {
      expect(OP_LABELS[op].phrase).toBeTruthy();
      expect(OP_LABELS[op].symbol).toBeTruthy();
    }
  });

  it("uses the exact phrasing from the UX spec", () => {
    expect(OP_LABELS[">"].phrase).toBe("is greater than");
    expect(OP_LABELS[">="].phrase).toBe("is at least");
    expect(OP_LABELS["<"].phrase).toBe("is less than");
    expect(OP_LABELS["<="].phrase).toBe("is at most");
    expect(OP_LABELS["=="].phrase).toBe("equals");
    expect(OP_LABELS["!="].phrase).toBe("does not equal");
  });
});

describe("opLabel", () => {
  it("resolves a known operator string to its label", () => {
    expect(opLabel(">=")).toEqual(OP_LABELS[">="]);
  });

  it("falls back to '>' for a value outside OPS instead of throwing", () => {
    expect(opLabel("bogus")).toEqual(OP_LABELS[">"]);
  });
});

describe("isComparableColumn", () => {
  it("accepts integer and float columns", () => {
    expect(isComparableColumn({ friendly_type: "integer" })).toBe(true);
    expect(isComparableColumn({ friendly_type: "float" })).toBe(true);
  });

  it("rejects string, date, datetime and boolean columns", () => {
    expect(isComparableColumn({ friendly_type: "string" })).toBe(false);
    expect(isComparableColumn({ friendly_type: "date" })).toBe(false);
    expect(isComparableColumn({ friendly_type: "datetime" })).toBe(false);
    expect(isComparableColumn({ friendly_type: "boolean" })).toBe(false);
  });

  it("degrades to comparable for a missing/unrecognized type rather than hiding the column", () => {
    expect(isComparableColumn({ friendly_type: "" })).toBe(true);
    expect(isComparableColumn({ friendly_type: undefined as unknown as string })).toBe(true);
    expect(isComparableColumn({ friendly_type: "some_future_connector_type" })).toBe(false);
  });
});

describe("formatComparisonValue", () => {
  it("formats a numeric string with locale grouping", () => {
    expect(formatComparisonValue("20000")).toBe((20000).toLocaleString());
  });

  it("passes through a non-numeric string unchanged", () => {
    expect(formatComparisonValue("abc")).toBe("abc");
  });
});

describe("monitor creation chains", () => {
  afterEach(() => vi.restoreAllMocks());

  const input = {
    dataSourceId: 3,
    queryName: "Q",
    sql: "SELECT 1 AS n",
    tags: ["t"],
    monitorName: "M",
    options: { column: "n", kind: "baseline" as const },
  };
  const mocks = () => ({
    create: vi.spyOn(api, "createQuery").mockResolvedValue({ id: 10, working_version: 2 } as never),
    publish: vi.spyOn(api, "publishQuery").mockResolvedValue({} as never),
    monitor: vi.spyOn(api, "createMonitor").mockResolvedValue({ id: 20 } as never),
    sub: vi.spyOn(api, "createMonitorSubscription").mockResolvedValue({} as never),
  });

  it("createMonitorFromSql runs create, publish, monitor, subscribe in order", async () => {
    const m = mocks();
    const order: string[] = [];
    m.create.mockImplementation(async () => (order.push("create"), { id: 10, working_version: 2 } as never));
    m.publish.mockImplementation(async () => (order.push("publish"), {} as never));
    m.monitor.mockImplementation(async () => (order.push("monitor"), { id: 20 } as never));
    m.sub.mockImplementation(async () => (order.push("sub"), {} as never));
    expect(await createMonitorFromSql(input)).toEqual({ queryId: 10, monitorId: 20, subscribed: true });
    expect(order).toEqual(["create", "publish", "monitor", "sub"]);
    expect(m.publish).toHaveBeenCalledWith(10, 2);
    expect(m.monitor).toHaveBeenCalledWith({ query_id: 10, name: "M", options: input.options });
  });

  it("resume skips the steps already done", async () => {
    const m = mocks();
    await createMonitorFromSql(input, { queryId: 10 });
    expect(m.create).not.toHaveBeenCalled();
    expect(m.publish).not.toHaveBeenCalled();
    expect(m.monitor).toHaveBeenCalledOnce();

    m.monitor.mockClear();
    await createMonitorFromSql(input, { queryId: 10, monitorId: 20 });
    expect(m.monitor).not.toHaveBeenCalled();
    expect(m.sub).toHaveBeenCalled();
  });

  it("a failure carries what was already written", async () => {
    const m = mocks();
    m.monitor.mockRejectedValue(new Error("nope"));
    const err = await createMonitorFromSql(input).catch((e) => e);
    expect(err).toBeInstanceOf(PartialCheckError);
    expect(err.partial).toEqual({ queryId: 10, workingVersion: undefined, monitorId: undefined });
  });

  it("a subscription failure still returns the monitor, unsubscribed", async () => {
    const m = mocks();
    m.sub.mockRejectedValue(new Error("no dest"));
    expect(await createMonitorFromSql(input)).toMatchObject({ monitorId: 20, subscribed: false });
  });

  it("subscribe: false skips the subscription; a destination and rearm pass through", async () => {
    const m = mocks();
    expect(await createMonitorFromSql({ ...input, subscribe: false })).toMatchObject({ monitorId: 20, subscribed: false });
    expect(m.sub).not.toHaveBeenCalled();
    await createMonitorFromSql({ ...input, rearm: 3600, subscribe: { destinationId: 7 } });
    expect(m.sub).toHaveBeenCalledWith(20, { destination_id: 7 });
    expect(m.monitor).toHaveBeenLastCalledWith({ query_id: 10, name: "M", options: input.options, rearm: 3600 });
  });

  it("createMonitorOnQuery creates then subscribes; propagates the create error", async () => {
    const m = mocks();
    expect(await createMonitorOnQuery(10, "M", input.options)).toEqual({ monitorId: 20, subscribed: true });
    expect(m.create).not.toHaveBeenCalled();
    m.sub.mockRejectedValue(new Error("x"));
    expect((await createMonitorOnQuery(10, "M", input.options)).subscribed).toBe(false);
    m.monitor.mockRejectedValue(new ApiError(400, "column missing"));
    await expect(createMonitorOnQuery(10, "M", input.options)).rejects.toBeInstanceOf(ApiError);
  });
});

describe("suggestionOptions", () => {
  const base: QueryMonitorSuggestion = {
    name: "n", kind: "baseline", column: "c", sensitivity: "high", op: null, value: null,
    definition: "", why: "", confidence: "high",
  };
  it("maps baseline", () => {
    expect(suggestionOptions(base)).toEqual({ kind: "baseline", column: "c", sensitivity: "high" });
  });
  it("maps threshold", () => {
    expect(suggestionOptions({ ...base, kind: "threshold", sensitivity: null, op: ">", value: 5 })).toEqual({
      kind: "threshold", column: "c", op: ">", value: 5,
    });
  });
});
