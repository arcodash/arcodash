import { describe, it, expect, afterEach, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useListView } from "./useListView";
import { mockViewport } from "@/__tests__/mockViewport";

const KEY = "list-view:test";

describe("useListView", () => {
  afterEach(() => {
    window.localStorage.removeItem(KEY);
  });

  it("defaults to table when nothing is stored", () => {
    const restore = mockViewport(true); // desktop
    try {
      const { result } = renderHook(() => useListView(KEY));
      expect(result.current.view).toBe("table");
      expect(result.current.storedView).toBe("table");
    } finally {
      restore();
    }
  });

  it("persists the chosen view to localStorage and restores it on next mount", () => {
    const restore = mockViewport(true); // desktop
    try {
      const { result, unmount } = renderHook(() => useListView(KEY));
      act(() => result.current.setView("cards"));
      expect(result.current.view).toBe("cards");
      expect(window.localStorage.getItem(KEY)).toBe("cards");

      unmount();

      const { result: result2 } = renderHook(() => useListView(KEY));
      expect(result2.current.view).toBe("cards");
      expect(result2.current.storedView).toBe("cards");
    } finally {
      restore();
    }
  });

  it("forces cards below the md breakpoint regardless of the stored preference", () => {
    const restoreDesktop = mockViewport(true);
    const { result, unmount } = renderHook(() => useListView(KEY));
    act(() => result.current.setView("table"));
    expect(result.current.view).toBe("table");
    unmount();
    restoreDesktop();

    // Same stored preference ("table"), but now the viewport reports mobile.
    const restoreMobile = mockViewport(false);
    try {
      const { result: mobileResult } = renderHook(() => useListView(KEY));
      expect(mobileResult.current.storedView).toBe("table");
      expect(mobileResult.current.view).toBe("cards");
      expect(mobileResult.current.isMobile).toBe(true);
    } finally {
      restoreMobile();
    }
  });

  it("defaults to table and doesn't throw when localStorage access is blocked (e.g. cookies disabled)", () => {
    const getItemSpy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("blocked", "SecurityError");
    });
    const setItemSpy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("blocked", "SecurityError");
    });
    const restore = mockViewport(true); // desktop
    try {
      const { result } = renderHook(() => useListView(KEY));
      expect(result.current.view).toBe("table");

      expect(() => act(() => result.current.setView("cards"))).not.toThrow();
      expect(result.current.view).toBe("cards");
    } finally {
      restore();
      getItemSpy.mockRestore();
      setItemSpy.mockRestore();
    }
  });
});
