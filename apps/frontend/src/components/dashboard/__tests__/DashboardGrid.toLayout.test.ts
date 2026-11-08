import { describe, it, expect } from "vitest";
import { toLayout } from "../DashboardGrid";
import type { Widget } from "@/types/api";

function widget(id: number, options: Widget["options"]): Widget {
  return {
    id,
    dashboard_id: 1,
    visualization_id: null,
    text: "x",
    width: 1,
    options,
    viewer_can_read_data: true,
    created_at: "2026-01-01T00:00:00.000Z",
    updated_at: "2026-01-01T00:00:00.000Z",
  };
}

describe("toLayout", () => {
  it("floors monitor widgets at two rows", () => {
    const [item] = toLayout([widget(1, { monitors: [5], position: { col: 0, row: 0, sizeX: 1, sizeY: 1 } })]);
    expect(item.h).toBe(2);
    expect(item.minH).toBe(2);
  });

  it("keeps a taller monitor widget's size", () => {
    const [item] = toLayout([widget(1, { monitors: [5], position: { col: 0, row: 0, sizeX: 1, sizeY: 4 } })]);
    expect(item.h).toBe(4);
  });

  it("leaves non-monitor widgets unchanged", () => {
    const [item] = toLayout([widget(2, { position: { col: 0, row: 0, sizeX: 1, sizeY: 1 } })]);
    expect(item.h).toBe(1);
    expect(item.minH).toBeUndefined();
  });
});
