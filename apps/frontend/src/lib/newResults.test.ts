import { describe, it, expect } from "vitest";
import { newResultsLabel, staleTileIds } from "./newResults";
import type { Widget } from "@/types/api";

function viz(id: number, retrievedAt: string, over: Partial<Widget> = {}, params: string[] = []): Widget {
  return {
    id,
    dashboard_id: 1,
    visualization_id: 10 + id,
    visualization: {
      id: 10 + id,
      query_id: id,
      type: "TABLE",
      name: "V",
      options: {},
      created_at: "2026-01-01T00:00:00Z",
      updated_at: "2026-01-01T00:00:00Z",
      query: { id, name: "Q", data_source_id: 1, parameter_names: params, options: {}, verified: false, is_published: true },
    },
    viewer_can_read_data: true,
    freshness: { retrieved_at: retrievedAt, expected_interval_secs: null, last_run_status: "success" },
    text: null,
    width: 1,
    options: {},
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    ...over,
  } as Widget;
}

const T10 = "2026-11-15T10:00:00Z";
const T11 = "2026-11-15T10:05:00Z";

describe("staleTileIds", () => {
  it("counts a tile with a later polled result", () => {
    expect(staleTileIds([viz(1, T11)], { 1: T10 }, {})).toEqual([1]);
  });

  it("equal or older never counts", () => {
    expect(staleTileIds([viz(1, T10), viz(2, T10)], { 1: T10, 2: T11 }, {})).toEqual([]);
  });

  it("skips unreadable, non-viz and parameterized tiles", () => {
    const text = viz(2, T11, { visualization_id: null, visualization: undefined });
    const unreadable = viz(1, T11, { viewer_can_read_data: false });
    const param = viz(3, T11, {}, ["region"]);
    expect(
      staleTileIds([unreadable, text, param], { 1: T10, 2: T10, 3: T10 }, { region: "EU" }),
    ).toEqual([]);
  });

  it("a tile that has not rendered yet never counts", () => {
    expect(staleTileIds([viz(1, T11)], {}, {})).toEqual([]);
  });
});

describe("newResultsLabel", () => {
  it("pluralizes", () => {
    expect(newResultsLabel(1)).toBe("New results available for 1 tile");
    expect(newResultsLabel(3)).toBe("New results available for 3 tiles");
  });
});
