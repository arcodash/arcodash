import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { api, ApiError } from "@/lib/api";
import { askFailureMessage, REFRESH_WAIT_CAP_MS, useAskRun } from "../askRun";

describe("askFailureMessage", () => {
  it("explains a dropped stream instead of the generic fallback", () => {
    expect(askFailureMessage(new TypeError("network error"))).toBe(
      "The connection to the AI dropped before it finished. Try again.",
    );
  });

  it("passes an API error's own message through", () => {
    expect(askFailureMessage(new ApiError(503, "The AI provider is busy right now."))).toBe(
      "The AI provider is busy right now.",
    );
  });

  it("keeps the generic fallback for unknown errors", () => {
    expect(askFailureMessage("boom")).toBe("Failed to generate SQL");
  });
});

describe("useAskRun profile refresh", () => {
  const body = { data_source_id: 7, question: "q" };
  const refreshingEvent = { type: "profile_refreshing", data_source_id: 7 } as const;
  const finalEvent = {
    type: "final", sql: "S", explanation: null, served_by: null, attempts: 0,
    tokens_in: 0, tokens_out: 0, executed: false, error: "boom",
  } as never;

  const schema = (refreshing: boolean, refresh_error: string | null = null) =>
    ({ schema: { tables: [] }, refreshing, refresh_error }) as never;

  beforeEach(() => vi.useFakeTimers());
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("waits, re-asks exactly once, and ignores a second profile_refreshing", async () => {
    const ask = vi.spyOn(api, "askStream").mockImplementation(async (_b, onEvent) => {
      onEvent(refreshingEvent);
      onEvent(finalEvent);
    });
    const poll = vi
      .spyOn(api, "getDataSourceSchema")
      .mockResolvedValueOnce(schema(true))
      .mockResolvedValue(schema(false));
    const { result } = renderHook(() => useAskRun());

    await act(async () => result.current.start(body));
    expect(result.current.state.waiting).toBe(true);
    expect(result.current.state.phase).toBe("running");
    expect(result.current.state.steps.map((s) => s.kind)).toEqual(["refreshing"]);

    await act(async () => { await vi.advanceTimersByTimeAsync(3000); });
    expect(poll).toHaveBeenCalledTimes(2);
    expect(ask).toHaveBeenCalledTimes(2);
    expect(ask.mock.calls[1][0]).toEqual(body);
    expect(result.current.state.phase).toBe("done");
    expect(result.current.state.steps.map((s) => s.kind)).toEqual(["refreshing", "retrying"]);

    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(ask).toHaveBeenCalledTimes(2);
  });

  it("gives up after the cap with the warning state", async () => {
    vi.spyOn(api, "askStream").mockImplementation(async (_b, onEvent) => onEvent(refreshingEvent));
    vi.spyOn(api, "getDataSourceSchema").mockResolvedValue(schema(true));
    const { result } = renderHook(() => useAskRun());
    await act(async () => result.current.start(body));
    await act(async () => { await vi.advanceTimersByTimeAsync(REFRESH_WAIT_CAP_MS + 1); });
    const s = result.current.state;
    expect([s.phase, s.giveUp, s.failReason, s.waiting]).toEqual(["failed", true, "profile_refreshing_timeout", false]);
  });

  it("fails with the admin message when the refresh errored", async () => {
    const ask = vi.spyOn(api, "askStream").mockImplementation(async (_b, onEvent) => onEvent(refreshingEvent));
    vi.spyOn(api, "getDataSourceSchema").mockResolvedValue(schema(false, "connection refused"));
    const { result } = renderHook(() => useAskRun());
    await act(async () => result.current.start(body));
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(result.current.state.phase).toBe("failed");
    expect(result.current.state.error).toContain("Ask an admin");
    expect(result.current.state.giveUp).toBe(false);
    expect(ask).toHaveBeenCalledTimes(1);
  });

  it("surfaces a budget-exceeded 429 message verbatim", async () => {
    const msg = "Monthly AI budget of $1.00 reached; resets 2026-11-01.";
    vi.spyOn(api, "askStream").mockRejectedValue(new ApiError(429, msg, "ai_budget_exceeded"));
    const { result } = renderHook(() => useAskRun());
    await act(async () => result.current.start(body));
    expect(result.current.state.phase).toBe("failed");
    expect(result.current.state.error).toBe(msg);
  });

  it("surfaces an in-stream budget failure verbatim", async () => {
    const msg = "Monthly AI budget of $1.00 reached; resets 2026-11-01.";
    vi.spyOn(api, "askStream").mockImplementation(async (_b, onEvent) =>
      onEvent({ type: "failed", message: msg, status: "failed" } as never));
    const { result } = renderHook(() => useAskRun());
    await act(async () => result.current.start(body));
    expect(result.current.state.phase).toBe("failed");
    expect(result.current.state.error).toBe(msg);
  });

  it("cancel stops polling", async () => {
    vi.spyOn(api, "askStream").mockImplementation(async (_b, onEvent) => onEvent(refreshingEvent));
    const poll = vi.spyOn(api, "getDataSourceSchema").mockResolvedValue(schema(true));
    const { result } = renderHook(() => useAskRun());
    await act(async () => result.current.start(body));
    act(() => result.current.cancel());
    const n = poll.mock.calls.length;
    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(poll).toHaveBeenCalledTimes(n);
    expect(result.current.state.phase).toBe("cancelled");
  });
});

describe("useAskRun widget stream", () => {
  const body = { data_source_id: 7, prompt: "p" };
  const finalEvent = {
    type: "final", sql: "S", explanation: null, served_by: "m", attempts: 1,
    tokens_in: 0, tokens_out: 0, executed: true, error: null,
  } as never;
  const created = {
    type: "created", query_id: 1, visualization_id: 2, widget_id: 3, viz_type: "CHART", row_count: 4,
  } as const;

  afterEach(() => vi.restoreAllMocks());

  const run = async (events: unknown[], hold = false) => {
    const fn = vi.fn(async (_b: typeof body, onEvent: (e: never) => void, signal?: AbortSignal) => {
      events.forEach((e) => onEvent(e as never));
      if (hold) await new Promise((_r, rej) => signal?.addEventListener("abort", () => rej(new DOMException("a", "AbortError"))));
    });
    const hook = renderHook(() => useAskRun(fn));
    await act(async () => hook.result.current.start(body, "dashboard"));
    return hook.result;
  };

  it("keeps running after final, shows the viz step, and ends on created", async () => {
    const result = await run([{ type: "draft", attempt: 1, sql: "S" }, finalEvent, { type: "visualizing" }, created]);
    expect(result.current.state.phase).toBe("done");
    expect(result.current.state.created).toEqual(created);
    expect(result.current.state.steps.map((s) => s.kind)).toEqual(["draft", "visualizing"]);
  });

  it("stays running between final and visualizing", async () => {
    const result = await run([finalEvent], true);
    expect(result.current.state.phase).toBe("running");
    expect(result.current.state.final).not.toBeNull();
  });

  it("fails when the stream reports failed after final", async () => {
    const result = await run([finalEvent, { type: "failed", status: "error", message: "nope" }]);
    expect(result.current.state.phase).toBe("failed");
    expect(result.current.state.created).toBeNull();
  });

  it("cancel after visualizing ignores a late created", async () => {
    const result = await run([finalEvent, { type: "visualizing" }], true);
    act(() => result.current.cancel());
    expect(result.current.state.phase).toBe("cancelled");
    expect(result.current.state.steps.map((s) => s.kind)).toEqual(["visualizing"]);
  });

  it("a plain ask still ends on final", async () => {
    vi.spyOn(api, "askStream").mockImplementation(async (_b, onEvent) => onEvent(finalEvent));
    const { result } = renderHook(() => useAskRun());
    await act(async () => result.current.start({ data_source_id: 7, question: "q" }));
    expect(result.current.state.phase).toBe("done");
  });
});

describe("useAskRun monitors_page run", () => {
  const suggestion = {
    name: "n", kind: "baseline", column: "c", sensitivity: "medium", op: null, value: null,
    definition: "", why: "", confidence: "high",
  };
  const final = {
    type: "final", sql: "S", served_by: "x", attempts: 1, tokens_in: 0, tokens_out: 0, executed: true,
  } as never;

  async function run(events: unknown[]) {
    const stream = vi.fn(async (_b: unknown, onEvent: (e: never) => void) => {
      for (const e of events) onEvent(e as never);
      await new Promise(() => {});
    });
    const { result } = renderHook(() => useAskRun<{ data_source_id: number; prompt: string }>(stream as never));
    await act(async () => result.current.start({ data_source_id: 1, prompt: "p" }, "monitors_page"));
    return result.current.state;
  }

  it("keeps running past final and shows the suggesting step", async () => {
    const s = await run([final, { type: "suggesting" }]);
    expect(s.phase).toBe("running");
    expect(s.steps.at(-1)).toEqual({ kind: "suggesting" });
  });

  it("finishes on suggested", async () => {
    const ev = { type: "suggested", sql: "S", data_source_id: 1, suggestion };
    const s = await run([final, { type: "suggesting" }, ev]);
    expect(s.phase).toBe("done");
    expect(s.suggested).toEqual(ev);
  });

  it("fails when failed arrives after final", async () => {
    const s = await run([final, { type: "failed", status: "generation_failed", message: "bad" }]);
    expect(s.phase).toBe("failed");
    expect(s.failReason).toBe("generation_failed");
  });
});
