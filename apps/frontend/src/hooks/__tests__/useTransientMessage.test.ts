import { describe, it, expect, vi, afterEach } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useTransientMessage } from "../useTransientMessage";

describe("useTransientMessage", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("clears the message after the window", () => {
    vi.useFakeTimers();
    const { result } = renderHook(() => useTransientMessage<string>(5000));

    act(() => result.current[1]("hello"));
    expect(result.current[0]).toBe("hello");

    act(() => vi.advanceTimersByTime(4999));
    expect(result.current[0]).toBe("hello");

    act(() => vi.advanceTimersByTime(1));
    expect(result.current[0]).toBeNull();
  });

  it("re-setting restarts the window instead of stacking a second timer", () => {
    vi.useFakeTimers();
    const { result } = renderHook(() => useTransientMessage<string>(5000));

    act(() => result.current[1]("first"));
    act(() => vi.advanceTimersByTime(3000));
    act(() => result.current[1]("second"));
    act(() => vi.advanceTimersByTime(3000));
    // The first timer would have fired by now (6s since "first") if it weren't cleared.
    expect(result.current[0]).toBe("second");

    act(() => vi.advanceTimersByTime(2000));
    expect(result.current[0]).toBeNull();
  });

  it("clears its timer on unmount", () => {
    vi.useFakeTimers();
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const { result, unmount } = renderHook(() => useTransientMessage<string>(5000));

    act(() => result.current[1]("hello"));
    unmount();

    act(() => vi.advanceTimersByTime(10000));

    // No "state update on an unmounted component" warning from a timer firing post-unmount.
    expect(errorSpy).not.toHaveBeenCalled();
    errorSpy.mockRestore();
  });
});
