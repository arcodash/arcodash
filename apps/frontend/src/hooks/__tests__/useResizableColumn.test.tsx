import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { ColumnResizeHandle } from "@/components/common/ColumnResizeHandle";
import { useResizableColumn } from "../useResizableColumn";

const KEY = "arcodash.listcol.test.name";

function Harness() {
  const col = useResizableColumn(KEY);
  return (
    <table>
      <thead>
        <tr>
          <th ref={col.headRef} data-testid="th" style={{ minWidth: col.width ?? undefined }}>
            Name
            <ColumnResizeHandle column={col} />
          </th>
        </tr>
      </thead>
    </table>
  );
}

const handle = () => screen.getByRole("separator", { name: "Resize Name column" });
const minWidth = () => screen.getByTestId("th").style.minWidth;

beforeEach(() => {
  vi.stubGlobal("PointerEvent", MouseEvent);
  window.localStorage.clear();
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({ width: 300 } as DOMRect);
  HTMLElement.prototype.setPointerCapture = vi.fn();
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("useResizableColumn", () => {
  it("arrow keys step 16px from the measured width, clamp at 160, and persist", () => {
    render(<Harness />);
    fireEvent.keyDown(handle(), { key: "ArrowRight" });
    expect(minWidth()).toBe("316px");
    expect(handle().getAttribute("aria-valuenow")).toBe("316");
    expect(window.localStorage.getItem(KEY)).toBe("316");
    fireEvent.keyDown(handle(), { key: "ArrowLeft" });
    expect(minWidth()).toBe("300px");
    for (let i = 0; i < 20; i++) fireEvent.keyDown(handle(), { key: "ArrowLeft" });
    expect(minWidth()).toBe("160px");
  });

  it("Home and double-click reset", () => {
    window.localStorage.setItem(KEY, "400");
    render(<Harness />);
    expect(minWidth()).toBe("400px");
    fireEvent.keyDown(handle(), { key: "Home" });
    expect(minWidth()).toBe("");
    expect(window.localStorage.getItem(KEY)).toBeNull();
    fireEvent.keyDown(handle(), { key: "ArrowRight" });
    fireEvent.doubleClick(handle());
    expect(minWidth()).toBe("");
    expect(window.localStorage.getItem(KEY)).toBeNull();
  });

  it("drag adds the pointer delta to the measured width, clamps, and persists on release", () => {
    render(<Harness />);
    fireEvent.pointerDown(handle(), { clientX: 100, pointerId: 1 });
    fireEvent.pointerMove(handle(), { clientX: 150, pointerId: 1 });
    expect(minWidth()).toBe("350px");
    fireEvent.pointerMove(handle(), { clientX: -500, pointerId: 1 });
    expect(minWidth()).toBe("160px");
    fireEvent.pointerMove(handle(), { clientX: 120, pointerId: 1 });
    fireEvent.pointerUp(handle(), { pointerId: 1 });
    expect(window.localStorage.getItem(KEY)).toBe("320");
  });

  it("a cancelled drag persists the latest width and clears dragging; later lost capture is a no-op", () => {
    render(<Harness />);
    fireEvent.pointerDown(handle(), { clientX: 100, pointerId: 1 });
    fireEvent.pointerMove(handle(), { clientX: 130, pointerId: 1 });
    expect(handle().firstElementChild!.classList.contains("bg-primary")).toBe(true);
    fireEvent.pointerCancel(handle(), { pointerId: 1 });
    expect(handle().firstElementChild!.classList.contains("bg-primary")).toBe(false);
    expect(window.localStorage.getItem(KEY)).toBe("330");
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    fireEvent.lostPointerCapture(handle(), { pointerId: 1 });
    expect(setItem).not.toHaveBeenCalled();
    expect(window.localStorage.getItem(KEY)).toBe("330");
  });

  it("exposes the measured width as aria-valuenow before any resize", () => {
    render(<Harness />);
    expect(handle().getAttribute("aria-valuenow")).toBe("300");
  });

  it("clamps a stored width below the minimum", () => {
    window.localStorage.setItem(KEY, "50");
    render(<Harness />);
    expect(minWidth()).toBe("160px");
  });

  it("renders and resizes when localStorage throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });
    render(<Harness />);
    expect(minWidth()).toBe("");
    fireEvent.keyDown(handle(), { key: "ArrowRight" });
    expect(minWidth()).toBe("316px");
  });

  it("does not let clicks reach the header", () => {
    const onClick = vi.fn();
    render(
      <div onClick={onClick}>
        <Harness />
      </div>,
    );
    fireEvent.click(handle());
    expect(onClick).not.toHaveBeenCalled();
  });
});
