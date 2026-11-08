/** DashboardGrid phone-stack branch (< md); the RGL desktop branch needs real DOM width and is
 * covered by e2e. */
import { afterEach, describe, it, expect, vi } from "vitest";
import { act, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DashboardGrid } from "@/components/dashboard/DashboardGrid";
import { api, ApiError } from "@/lib/api";
import type { Widget } from "@/types/api";
import type { ReactElement } from "react";

function renderGrid(ui: ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

type MockLayoutItem = { i: string; x: number; y: number; w: number; h: number };

// Captures the gesture-stop callbacks wired to <ReactGridLayout>; jsdom can't drive a drag.
type StopCb = (layout: MockLayoutItem[]) => void;
let lastOnDragStop: StopCb | null = null;
let lastOnResizeStop: StopCb | null = null;
let lastDragConfig: { handle?: string; cancel?: string; bounded?: boolean } | null = null;
let lastGridConfig: { containerPadding?: readonly [number, number] } | null = null;

vi.mock("react-grid-layout", () => ({
  __esModule: true,
  default: (props: {
    onDragStop?: StopCb;
    onResizeStop?: StopCb;
    dragConfig?: { handle?: string; cancel?: string; bounded?: boolean };
    gridConfig?: { containerPadding?: readonly [number, number] };
  }) => {
    lastGridConfig = props.gridConfig ?? null;
    lastOnDragStop = props.onDragStop ?? null;
    lastOnResizeStop = props.onResizeStop ?? null;
    lastDragConfig = props.dragConfig ?? null;
    return <div data-testid="rgl">RGL</div>;
  },
  useContainerWidth: () => ({
    width: 800,
    mounted: true,
    containerRef: { current: null },
    measureWidth: () => {},
  }),
}));

function mockDesktopViewport() {
  const original = window.matchMedia;
  window.matchMedia = ((query: string) => ({
    matches: true,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
  return () => {
    window.matchMedia = original;
  };
}

function makeWidget(id: number, row: number, col: number, text: string): Widget {
  return {
    id,
    dashboard_id: 1,
    visualization_id: null,
    query_result_data: null,
    viewer_can_read_data: true,
    text,
    width: 1,
    options: { position: { col, row, sizeX: 3, sizeY: 2 } },
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
  };
}

describe("DashboardGrid phone stack", () => {
  it("renders a vertical stack ordered by (row, col) with no RGL when matchMedia reports < md", () => {
    const widgets = [
      makeWidget(1, 1, 0, "second row"),
      makeWidget(2, 0, 1, "first row, second col"),
      makeWidget(3, 0, 0, "first row, first col"),
    ];

    renderGrid(
      <DashboardGrid
        widgets={widgets}
        editMode={false}
        onDeleteWidget={vi.fn()}
      />,
    );

    const stack = screen.getByTestId("dashboard-stack");
    expect(stack).toBeInTheDocument();
    expect(screen.queryByTestId("rgl")).not.toBeInTheDocument();
    expect(document.querySelector(".react-grid-layout")).not.toBeInTheDocument();

    const texts = Array.from(stack.querySelectorAll("p")).map((el) => el.textContent);
    expect(texts).toEqual([
      "first row, first col",
      "first row, second col",
      "second row",
    ]);
  });

  it("gives each mobile-stack slot a concrete pixel height, not just a min-height", () => {
    // `min-height` alone gives a percentage-height chart no box to resolve against; assert the
    // inline height scaled from sizeY (jsdom can't see the 0px collapse).
    const widgets = [makeWidget(1, 0, 0, "only widget")];
    renderGrid(
      <DashboardGrid widgets={widgets} editMode={false} onDeleteWidget={vi.fn()} />,
    );
    const stack = screen.getByTestId("dashboard-stack");
    const slot = stack.firstElementChild as HTMLElement;
    expect(slot.style.height).toBe("200px");
    expect(slot.className).not.toContain("min-h-");
  });

  it("a taller widget (sizeY) gets a taller mobile slot, floored at 200px", () => {
    const tall: Widget = {
      ...makeWidget(1, 0, 0, "tall widget"),
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 4 } },
    };
    renderGrid(<DashboardGrid widgets={[tall]} editMode={false} onDeleteWidget={vi.fn()} />);
    const stack = screen.getByTestId("dashboard-stack");
    const slot = stack.firstElementChild as HTMLElement;
    expect(slot.style.height).toBe("400px");
  });

  it("wires editMode add/delete callbacks in the stack (no drag, no RGL)", () => {
    const onDeleteWidget = vi.fn();
    const widgets = [makeWidget(1, 0, 0, "only widget")];

    renderGrid(
      <DashboardGrid
        widgets={widgets}
        editMode
        onDeleteWidget={onDeleteWidget}
      />,
    );

    const deleteBtn = screen
      .getAllByRole("button")
      .find((b) => b.className.includes("text-destructive"));
    expect(deleteBtn).toBeTruthy();
    deleteBtn!.click();
    expect(onDeleteWidget).toHaveBeenCalledWith(1);
  });

  it("gives every icon-only widget control a non-empty accessible name", () => {
    const widgets = [makeWidget(1, 0, 0, "first widget"), makeWidget(2, 0, 1, "second widget")];

    renderGrid(
      <DashboardGrid widgets={widgets} editMode onDeleteWidget={vi.fn()} />,
    );

    const buttons = screen.getAllByRole("button");
    expect(buttons.length).toBeGreaterThan(0);
    for (const button of buttons) {
      expect(button).toHaveAccessibleName();
    }
  });
});

describe("DashboardGrid desktop layout save (batched PUT /layout)", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  function renderDesktop(widgets: Widget[], editMode = true) {
    const restore = mockDesktopViewport();
    const utils = renderGrid(
      <DashboardGrid
        widgets={widgets}
        editMode={editMode}
        onDeleteWidget={vi.fn()}
        dashboardSlug="sales"
      />,
    );
    return { ...utils, restore };
  }

  it("restricts dragging to the tile header and exempts interactive controls", () => {
    const { restore } = renderDesktop([makeWidget(1, 0, 0, "a")]);
    expect(lastDragConfig).toMatchObject({ handle: ".tile-drag-handle", bounded: true });
    expect(lastDragConfig?.cancel).toContain("button");
    restore();
  });

  it("issues exactly one saveDashboardLayout call carrying every widget's position after a multi-tile drag", async () => {
    const saveSpy = vi.spyOn(api, "saveDashboardLayout").mockResolvedValue([]);
    const { restore } = renderDesktop([makeWidget(1, 0, 0, "a"), makeWidget(2, 0, 1, "b")]);

    lastOnDragStop!([
      { i: "1", x: 2, y: 1, w: 3, h: 2 },
      { i: "2", x: 4, y: 1, w: 3, h: 2 },
    ]);

    await waitFor(() => expect(saveSpy).toHaveBeenCalledTimes(1));
    expect(saveSpy).toHaveBeenCalledWith("sales", [
      { widget_id: 1, position: { col: 2, row: 1, sizeX: 3, sizeY: 2 } },
      { widget_id: 2, position: { col: 4, row: 1, sizeX: 3, sizeY: 2 } },
    ]);
    restore();
  });

  it("passes zero container padding so a bounded drag cannot overshoot the grid edge", () => {
    const { restore } = renderDesktop([makeWidget(1, 0, 0, "a")]);
    expect(lastGridConfig?.containerPadding).toEqual([0, 0]);
    restore();
  });

  it("starts the second layout save only after the first settles", async () => {
    let resolveFirst!: () => void;
    const saveSpy = vi
      .spyOn(api, "saveDashboardLayout")
      .mockImplementationOnce(() => new Promise((r) => { resolveFirst = () => r([]); }))
      .mockResolvedValue([]);
    const { restore } = renderDesktop([makeWidget(1, 0, 0, "a")]);

    act(() => lastOnDragStop!([{ i: "1", x: 2, y: 1, w: 3, h: 2 }]));
    act(() => lastOnDragStop!([{ i: "1", x: 4, y: 1, w: 3, h: 2 }]));
    await act(async () => {});
    expect(saveSpy).toHaveBeenCalledTimes(1);

    await act(async () => resolveFirst());
    await waitFor(() => expect(saveSpy).toHaveBeenCalledTimes(2));
    restore();
  });

  it("saves on resize stop", async () => {
    const saveSpy = vi.spyOn(api, "saveDashboardLayout").mockResolvedValue([]);
    const { restore } = renderDesktop([makeWidget(1, 0, 0, "a")]);

    lastOnResizeStop!([{ i: "1", x: 0, y: 0, w: 4, h: 3 }]);

    await waitFor(() => expect(saveSpy).toHaveBeenCalled());
    expect(saveSpy).toHaveBeenCalledWith("sales", [
      { widget_id: 1, position: { col: 0, row: 0, sizeX: 4, sizeY: 3 } },
    ]);
    restore();
  });

  it("does not save when the reported layout matches the widgets' current positions", () => {
    const saveSpy = vi.spyOn(api, "saveDashboardLayout").mockResolvedValue([]);
    const { restore } = renderDesktop([makeWidget(1, 0, 0, "a")]);

    lastOnDragStop!([{ i: "1", x: 0, y: 0, w: 3, h: 2 }]);

    expect(saveSpy).not.toHaveBeenCalled();
    restore();
  });

  it("does not save outside edit mode", () => {
    const saveSpy = vi.spyOn(api, "saveDashboardLayout").mockResolvedValue([]);
    const { restore } = renderDesktop([makeWidget(1, 0, 0, "a")], false);

    lastOnDragStop!([{ i: "1", x: 2, y: 1, w: 3, h: 2 }]);

    expect(saveSpy).not.toHaveBeenCalled();
    restore();
  });

  it("a failed layout save renders a destructive error above the grid", async () => {
    vi.spyOn(api, "saveDashboardLayout").mockRejectedValue(new ApiError(500, "Layout save failed"));
    const { restore } = renderDesktop([makeWidget(1, 0, 0, "a")]);

    act(() => lastOnDragStop!([{ i: "1", x: 2, y: 1, w: 3, h: 2 }]));

    expect(await screen.findByText("Layout save failed")).toBeInTheDocument();
    restore();
  });
});

describe("phone stack edit mode", () => {
  it("renders no drag handle, grip or select-none, but keeps Remove", () => {
    renderGrid(
      <DashboardGrid
        widgets={[makeWidget(1, 0, 0, "tile")]}
        editMode
        onDeleteWidget={vi.fn()}
      />,
    );
    const stack = screen.getByTestId("dashboard-stack");
    expect(stack.querySelector(".tile-drag-handle")).not.toBeInTheDocument();
    expect(stack.querySelector("[data-testid='widget-title-group']")).not.toBeInTheDocument();
    expect(stack.querySelector(".lucide-grip-vertical")).not.toBeInTheDocument();
    expect(stack.querySelector(".select-none")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^Remove / })).toBeInTheDocument();
  });
});
