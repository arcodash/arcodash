import { describe, it, expect, vi, afterEach } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useAutosave, type AutosaveOutcome } from "../useAutosave";
import { ConflictError } from "@/lib/api";

function setOnline(value: boolean) {
  Object.defineProperty(window.navigator, "onLine", { value, configurable: true });
}

describe("useAutosave", () => {
  afterEach(() => {
    vi.useRealTimers();
    setOnline(true);
  });

  it("debounce-saves once after typing settles, then shows saved", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);
    const onConflict = vi.fn();

    const { result, rerender } = renderHook(
      ({ payload }) =>
        useAutosave({ enabled: true, payload, save, onConflict, debounceMs: 1000 }),
      { initialProps: { payload: "SELECT 1" } },
    );

    expect(result.current.status).toBe("idle");

    rerender({ payload: "SELECT 1, 2" });
    rerender({ payload: "SELECT 1, 2, 3" });

    act(() => {
      vi.advanceTimersByTime(500);
    });
    expect(save).not.toHaveBeenCalled();

    act(() => {
      vi.advanceTimersByTime(500);
    });
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save).toHaveBeenCalledWith("SELECT 1, 2, 3");

    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
  });

  it("demotes status away from 'saved' the instant a new edit is queued, before the debounce fires", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));

    rerender({ payload: "c" });
    expect(result.current.status).not.toBe("saved");

    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("saving"));
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
  });

  it("does not save when disabled", () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);
    const { rerender } = renderHook(
      ({ payload, enabled }) =>
        useAutosave({ enabled, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a", enabled: false } },
    );
    rerender({ payload: "b", enabled: false });
    act(() => vi.advanceTimersByTime(2000));
    expect(save).not.toHaveBeenCalled();
  });

  it("shows failed + retry on a rejected save, and retry resends the same buffer", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockRejectedValueOnce(new Error("boom")).mockResolvedValueOnce(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("failed"));
    expect(result.current.hasUnpersistedEdits).toBe(true);

    act(() => {
      result.current.retry();
    });
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(save).toHaveBeenCalledTimes(2);
    expect(save).toHaveBeenLastCalledWith("b");
  });

  it("goes offline instead of attempting a save while navigator.onLine is false, and retries on reconnect", async () => {
    vi.useFakeTimers();
    setOnline(false);
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));

    await vi.waitFor(() => expect(result.current.status).toBe("offline"));
    expect(save).not.toHaveBeenCalled();
    expect(result.current.hasUnpersistedEdits).toBe(true);

    setOnline(true);
    act(() => {
      window.dispatchEvent(new Event("online"));
    });
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(save).toHaveBeenCalledWith("b");
  });

  it("hands a ConflictError to onConflict instead of flipping to failed", async () => {
    vi.useFakeTimers();
    const conflict = new ConflictError("stale", 9);
    const save = vi.fn().mockRejectedValue(conflict);
    const onConflict = vi.fn();

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));

    await vi.waitFor(() => expect(onConflict).toHaveBeenCalledWith(conflict));
    expect(result.current.status).not.toBe("failed");
  });

  it("does not treat the initial payload (already enabled on mount) as a pending edit", () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);
    renderHook(() => useAutosave({ enabled: true, payload: "already saved", save, onConflict: vi.fn() }));
    act(() => vi.advanceTimersByTime(2000));
    expect(save).not.toHaveBeenCalled();
  });

  it("terminalizes to a conflict status (not failed or stuck saving), pausing further saves until clearConflict", async () => {
    vi.useFakeTimers();
    const conflict = new ConflictError("stale", 9);
    const save = vi.fn().mockRejectedValueOnce(conflict).mockResolvedValueOnce(undefined);
    const onConflict = vi.fn();

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));

    await vi.waitFor(() => expect(result.current.status).toBe("conflict"));
    expect(result.current.status).not.toBe("failed");
    expect(result.current.hasUnpersistedEdits).toBe(true);

    rerender({ payload: "c" });
    act(() => vi.advanceTimersByTime(2000));
    expect(save).toHaveBeenCalledTimes(1);

    act(() => result.current.clearConflict());
    rerender({ payload: "d" });
    act(() => vi.advanceTimersByTime(1000));

    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save).toHaveBeenLastCalledWith("d");
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
  });

  it("flushes a pending debounced edit on unmount instead of dropping it", () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);
    const { unmount, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });

    act(() => vi.advanceTimersByTime(500));
    expect(save).not.toHaveBeenCalled();

    unmount();
    expect(save).toHaveBeenCalledWith("b");
  });

  it("does not flush anything on unmount once an edit has already saved", () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);
    const { unmount, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));

    unmount();
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("unmount during an in-flight save fires no second, concurrent request", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { unmount, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "A" });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);
    expect(save).toHaveBeenCalledWith("A");

    rerender({ payload: "B" });

    unmount();
    // Unmounting with "B" pending must not fire a request racing "A"'s version bump.
    expect(save).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveSave();
    });
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save).toHaveBeenLastCalledWith("B");
  });

  it("forceSave saves the current payload immediately, skipping the debounce", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });

    await act(() => result.current.forceSave());
    expect(save).toHaveBeenCalledTimes(1);
    expect(save).toHaveBeenCalledWith("b");

    act(() => vi.advanceTimersByTime(2000));
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("forceSave is a no-op during a conflict, same as the debounce path", async () => {
    vi.useFakeTimers();
    const conflict = new ConflictError("stale", 9);
    const save = vi.fn().mockRejectedValueOnce(conflict);
    const onConflict = vi.fn();

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("conflict"));

    await act(() => result.current.forceSave());
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("forceSave awaits a save already in flight instead of racing it, then skips a redundant save once it lands unchanged", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);
    expect(result.current.status).toBe("saving");

    let forceSavePromise!: Promise<AutosaveOutcome>;
    act(() => {
      forceSavePromise = result.current.forceSave();
    });
    expect(save).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveSave();
      await forceSavePromise;
    });

    expect(save).toHaveBeenCalledTimes(1);
    expect(result.current.status).toBe("saved");
  });

  it("forceSave clears the pending debounce timer before awaiting the in-flight save, so the timer can't sneak in an extra save mid-await", async () => {
    vi.useFakeTimers();
    let resolveFirstSave: () => void = () => {};
    const save = vi
      .fn()
      .mockImplementationOnce(
        () =>
          new Promise<void>((resolve) => {
            resolveFirstSave = resolve;
          }),
      )
      .mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) =>
        useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a" } },
    );

    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    rerender({ payload: "c" });

    let forceSavePromise!: Promise<AutosaveOutcome>;
    act(() => {
      forceSavePromise = result.current.forceSave();
    });

    // If forceSave hadn't cleared "c"'s timer before awaiting "b", this would fire a redundant
    // save.
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveFirstSave();
      await forceSavePromise;
    });

    expect(save).toHaveBeenCalledTimes(2);
    expect(save).toHaveBeenNthCalledWith(1, "b");
    expect(save).toHaveBeenNthCalledWith(2, "c");
  });

  it("forceSave is a no-op when the payload hasn't changed since the last save", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(save).toHaveBeenCalledTimes(1);

    await act(() => result.current.forceSave());
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("forceSave saves a never-saved payload even while disabled", async () => {
    const save = vi.fn().mockResolvedValue(undefined);

    const { result } = renderHook(() =>
      useAutosave({ enabled: false, payload: "untouched scaffold", save, onConflict: vi.fn() }),
    );

    await act(() => result.current.forceSave());
    expect(save).toHaveBeenCalledWith("untouched scaffold");
  });

  it("baselineOnEnable: false schedules a debounced save of the enabling payload instead of treating it as already-saved", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { rerender } = renderHook(
      ({ enabled, payload }) =>
        useAutosave({ enabled, payload, save, onConflict: vi.fn(), baselineOnEnable: false }),
      { initialProps: { enabled: false, payload: "first keystroke" } },
    );

    rerender({ enabled: true, payload: "first keystroke" });

    act(() => {
      vi.advanceTimersByTime(1000);
    });
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(1));
    expect(save).toHaveBeenCalledWith("first keystroke");
  });

  it("baselineOnEnable defaults to true — the enabling payload is still adopted as the baseline unchanged from before", () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { rerender } = renderHook(
      ({ enabled, payload }) => useAutosave({ enabled, payload, save, onConflict: vi.fn() }),
      { initialProps: { enabled: false, payload: "already there" } },
    );

    rerender({ enabled: true, payload: "already there" });
    act(() => vi.advanceTimersByTime(2000));
    expect(save).not.toHaveBeenCalled();
  });

  it("a disable -> enable round trip does not adopt an edited buffer (clear-then-paste)", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { rerender } = renderHook(
      ({ enabled, payload }) =>
        useAutosave({ enabled, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { enabled: true, payload: "Original" } },
    );

    rerender({ enabled: false, payload: "" });

    // Paste flips `enabled` and the payload in one render; the existing baseline must be left
    // alone.
    rerender({ enabled: true, payload: "Pasted Name" });

    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(save).toHaveBeenCalledWith("Pasted Name"));
  });

  it("clearConflict does not clear the baseline, so a later re-enable can't adopt an edit", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ enabled, payload }) =>
        useAutosave({ enabled, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { enabled: true, payload: "Original" } },
    );

    // Conflict resolution resets pending state; the saved baseline must survive or the enable round
    // trip swallows the edit.
    act(() => {
      result.current.clearConflict();
    });

    rerender({ enabled: false, payload: "" });
    rerender({ enabled: true, payload: "Pasted Name" });

    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(save).toHaveBeenCalledWith("Pasted Name"));
  });

  it("first enable still adopts the loaded payload as the baseline (no regression)", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { rerender } = renderHook(
      ({ enabled, payload }) =>
        useAutosave({ enabled, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { enabled: false, payload: "Loaded From Server" } },
    );

    rerender({ enabled: true, payload: "Loaded From Server" });

    act(() => vi.advanceTimersByTime(2000));
    expect(save).not.toHaveBeenCalled();
  });

  it("markClean adopts the payload of the same commit as the baseline, without saving it", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );

    // React 18 batches rehydrate + markClean into one commit; the adopting effect sees the
    // rehydrated payload.
    act(() => {
      result.current.markClean();
      rerender({ payload: "server copy" });
    });

    act(() => vi.advanceTimersByTime(2000));
    expect(save).not.toHaveBeenCalled();

    rerender({ payload: "server copy, edited" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(save).toHaveBeenCalledWith("server copy, edited"));
  });

  it("markClean still suppresses a resave when the rehydrated payload is byte-identical to what's on screen", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));

    act(() => {
      result.current.markClean();
      rerender({ payload: "b" });
    });

    act(() => vi.advanceTimersByTime(2000));
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("markClean() with a byte-identical payload does not swallow the next, unrelated edit", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(save).toHaveBeenCalledTimes(1);

    act(() => {
      result.current.markClean();
    });

    rerender({ payload: "c" });
    expect(result.current.hasUnflushedChanges()).toBe(true);

    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(save).toHaveBeenCalledWith("c"));
  });

  it("markClean() with no other state change still forces the baseline to be reconfirmed", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a" } },
    );
    expect(result.current.status).toBe("idle");

    // No payload change and status already idle: setStatus is a no-op, so only an effect without a
    // dependency array would catch this.
    act(() => {
      result.current.markClean();
    });

    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(save).toHaveBeenCalledWith("b"));
  });

  it("hasUnflushedChanges is true during the debounce window, before status ever flips to saving", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: "a" } },
    );
    expect(result.current.hasUnflushedChanges()).toBe(false);

    rerender({ payload: "b" });
    expect(result.current.status).not.toBe("saving");
    expect(result.current.hasUnflushedChanges()).toBe(true);

    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(result.current.hasUnflushedChanges()).toBe(false);
  });

  it("hasUnpersistedEdits is true while a save is genuinely in flight, not just dispatched", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    expect(result.current.status).toBe("saving");
    expect(result.current.hasUnpersistedEdits).toBe(true);

    await act(async () => {
      resolveSave();
    });
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(result.current.hasUnpersistedEdits).toBe(false);
  });

  it("hasUnflushedChanges stays true while a save is in flight and while failed, false once saved", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    expect(result.current.status).toBe("saving");
    expect(result.current.hasUnflushedChanges()).toBe(true);

    await act(async () => {
      resolveSave();
    });
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(result.current.hasUnflushedChanges()).toBe(false);
  });

  it("does not schedule a save when rerendered with the same payload reference", () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);
    const payload = { name: "q", query: "SELECT 1" };

    const { rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload } },
    );

    rerender({ payload });
    act(() => vi.advanceTimersByTime(2000));

    expect(save).not.toHaveBeenCalled();
  });

  it("does not schedule a save when rerendered with a new object that serializes identically", () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: { name: "q", query: "SELECT 1" } } },
    );

    rerender({ payload: { name: "q", query: "SELECT 1" } });
    act(() => vi.advanceTimersByTime(2000));

    expect(save).not.toHaveBeenCalled();
  });

  it("schedules a save when the rerendered payload's contents genuinely differ", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockResolvedValue(undefined);

    const { rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: { name: "q", query: "SELECT 1" } } },
    );

    rerender({ payload: { name: "q", query: "SELECT 2" } });
    act(() => vi.advanceTimersByTime(1000));

    await vi.waitFor(() =>
      expect(save).toHaveBeenCalledWith(expect.objectContaining({ query: "SELECT 2" })),
    );
  });

  it("an edit that arrives during an in-flight save is not dropped", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: { name: "q", query: "a" } } },
    );
    rerender({ payload: { name: "q", query: "b" } });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);
    expect(save).toHaveBeenCalledWith(expect.objectContaining({ query: "b" }));

    // "c" lands while "b" is in flight; an unconditional `pendingRef.current = null` would drop it.
    rerender({ payload: { name: "q", query: "c" } });
    expect(result.current.hasUnflushedChanges()).toBe(true);

    await act(async () => {
      resolveSave();
    });
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save).toHaveBeenLastCalledWith(expect.objectContaining({ query: "c" }));
    expect(result.current.hasUnflushedChanges()).toBe(true);

    await act(async () => {
      resolveSave();
    });
    await vi.waitFor(() => expect(result.current.status).toBe("saved"));
    expect(result.current.hasUnflushedChanges()).toBe(false);
  });

  it("the chained follow-up save fires from the prior save resolving alone, with no further debounce tick", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: { name: "q", query: "a" } } },
    );
    rerender({ payload: { name: "q", query: "b" } });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    rerender({ payload: { name: "q", query: "c" } });
    await act(async () => {
      resolveSave();
    });

    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save).toHaveBeenLastCalledWith(expect.objectContaining({ query: "c" }));
  });

  it("does not fire a second, concurrent save while one is already in flight (no fabricated conflict)", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { rerender } = renderHook(
      ({ payload }) =>
        useAutosave({ enabled: true, payload, save, onConflict: vi.fn(), debounceMs: 1000 }),
      { initialProps: { payload: { name: "q", query: "a" } } },
    );
    rerender({ payload: { name: "q", query: "b" } });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    rerender({ payload: { name: "q", query: "c" } });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveSave();
    });
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save).toHaveBeenLastCalledWith(expect.objectContaining({ query: "c" }));
  });

  it("retry does not fire a second save while one is already in flight", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    rerender({ payload: "c" });
    act(() => {
      result.current.retry();
    });
    expect(save).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveSave();
    });
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
  });

  it("clearConflict returns the indicator to idle and disarms the blocker", async () => {
    vi.useFakeTimers();
    const conflict = new ConflictError("stale", 9);
    const save = vi.fn().mockRejectedValueOnce(conflict);
    const onConflict = vi.fn();

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("conflict"));
    expect(result.current.hasUnpersistedEdits).toBe(true);

    act(() => {
      result.current.clearConflict();
    });

    expect(result.current.status).toBe("idle");
    expect(result.current.hasUnpersistedEdits).toBe(false);
    expect(result.current.hasUnflushedChanges()).toBe(false);
  });

  it("markClean clears the pending buffer and the failed status", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockRejectedValueOnce(new Error("boom"));

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("failed"));
    expect(result.current.hasUnflushedChanges()).toBe(true);

    act(() => {
      result.current.markClean();
    });

    expect(result.current.status).toBe("idle");
    expect(result.current.hasUnflushedChanges()).toBe(false);
  });

  it("a stale Retry can't resurrect a discarded draft", async () => {
    vi.useFakeTimers();
    const save = vi.fn().mockRejectedValueOnce(new Error("boom"));

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(result.current.status).toBe("failed"));

    act(() => {
      result.current.markClean();
    });

    act(() => {
      result.current.retry();
    });
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("a save in flight across markClean writes nothing back", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    expect(result.current.status).toBe("saving");

    act(() => {
      result.current.markClean();
    });
    expect(result.current.status).toBe("idle");

    rerender({ payload: "server copy" });

    await act(async () => {
      resolveSave();
    });

    expect(result.current.status).toBe("idle");
    expect(save).toHaveBeenCalledTimes(1);

    rerender({ payload: "server copy, edited" });
    act(() => vi.advanceTimersByTime(1000));
    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save).toHaveBeenLastCalledWith("server copy, edited");
  });

  it("a save that fails across markClean does not flip the indicator to failed", async () => {
    vi.useFakeTimers();
    let rejectSave: (err: Error) => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((_resolve, reject) => {
          rejectSave = reject;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: "a" } },
    );
    rerender({ payload: "b" });
    act(() => vi.advanceTimersByTime(1000));
    expect(result.current.status).toBe("saving");

    act(() => {
      result.current.markClean();
    });
    expect(result.current.status).toBe("idle");

    rerender({ payload: "server copy" });

    await act(async () => {
      rejectSave(new Error("boom"));
    });

    expect(result.current.status).toBe("idle");
  });

  it("a save in flight across markClean does not race a concurrent send of the next edit", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: { name: "q", query: "a" } } },
    );
    rerender({ payload: { name: "q", query: "b" } });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    act(() => {
      result.current.markClean();
    });

    rerender({ payload: { name: "q", query: "c" } });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveSave();
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(save).toHaveBeenCalledTimes(2);
    expect(save).toHaveBeenLastCalledWith(expect.objectContaining({ query: "c" }));
  });

  it("does not strand an edit queued mid-flight across markClean — the timer re-arms instead of dying with no owner", async () => {
    vi.useFakeTimers();
    let resolveSave: () => void = () => {};
    const save = vi.fn().mockImplementation(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );

    const { result, rerender } = renderHook(
      ({ payload }) => useAutosave({ enabled: true, payload, save, onConflict: vi.fn() }),
      { initialProps: { payload: { name: "q", query: "a" } } },
    );
    rerender({ payload: { name: "q", query: "b" } });
    act(() => vi.advanceTimersByTime(1000));
    expect(save).toHaveBeenCalledTimes(1);

    act(() => {
      result.current.markClean();
    });
    rerender({ payload: { name: "q", query: "c" } });
    act(() => vi.advanceTimersByTime(1000));
    expect(result.current.hasUnflushedChanges()).toBe(true);

    await act(async () => {
      resolveSave();
    });
    // The superseded save chains nothing; the re-armed timer sends "c".
    act(() => vi.advanceTimersByTime(1000));

    await vi.waitFor(() => expect(save).toHaveBeenCalledTimes(2));
    expect(save).toHaveBeenLastCalledWith(expect.objectContaining({ query: "c" }));

    await act(async () => {
      resolveSave();
    });
    await vi.waitFor(() => expect(result.current.hasUnflushedChanges()).toBe(false));
  });
});
