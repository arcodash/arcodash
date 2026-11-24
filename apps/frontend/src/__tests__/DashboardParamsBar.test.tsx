import { describe, it, expect } from "vitest";
import {
  resolveDashboardParams,
  parseValuesFromSearch,
  serializeValuesToSearch,
} from "@/components/dashboard/DashboardParamsBar";
import type { Widget } from "@/types/api";

function makeWidget(overrides: Partial<Widget> & { id: number }): Widget {
  return {
    dashboard_id: 1,
    visualization_id: 1,
    text: null,
    width: 1,
    options: {},
    viewer_can_read_data: true,
    created_at: "",
    updated_at: "",
    ...overrides,
  };
}

describe("resolveDashboardParams", () => {
  it("first widget to define a token wins", () => {
    const widgets = [
      makeWidget({
        id: 1,
        visualization: {
          id: 1,
          query_id: 10,
          type: "table",
          name: "Widget A",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 10,
            name: "q1",
            data_source_id: 1,
            parameter_names: ["region"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "region", title: "Region", type: "dropdown", enum_options: "us\neu" }] },
          },
        },
      }),
      makeWidget({
        id: 2,
        visualization: {
          id: 2,
          query_id: 11,
          type: "table",
          name: "Widget B",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 11,
            name: "q2",
            data_source_id: 1,
            parameter_names: ["region"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "region", title: "Region (other)", type: "dropdown", enum_options: "us" }] },
          },
        },
      }),
    ];

    const resolved = resolveDashboardParams(widgets);
    expect(resolved).toHaveLength(1);
    expect(resolved[0].definition?.title).toBe("Region");
    expect(resolved[0].queryId).toBe(10);
  });

  it("records a conflict when a later widget defines the same token with a different type, without overwriting the winner", () => {
    const widgets = [
      makeWidget({
        id: 1,
        visualization: {
          id: 1,
          query_id: 10,
          type: "table",
          name: "Widget A",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 10,
            name: "q1",
            data_source_id: 1,
            parameter_names: ["d"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "d", title: "Date", type: "date" }] },
          },
        },
      }),
      makeWidget({
        id: 2,
        visualization: {
          id: 2,
          query_id: 11,
          type: "table",
          name: "Widget B",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 11,
            name: "q2",
            data_source_id: 1,
            parameter_names: ["d"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "d", title: "Date range", type: "date-range" }] },
          },
        },
      }),
    ];

    const resolved = resolveDashboardParams(widgets);
    expect(resolved).toHaveLength(1);
    expect(resolved[0].definition?.type).toBe("date");
    expect(resolved[0].queryId).toBe(10);
    expect(resolved[0].conflict).toEqual({ widgetName: "Widget B", type: "date-range", sameType: false });
  });

  it("records a conflict when a later widget defines the same token, same type, with different enum_options", () => {
    const widgets = [
      makeWidget({
        id: 1,
        visualization: {
          id: 1,
          query_id: 10,
          type: "table",
          name: "Widget A",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 10,
            name: "q1",
            data_source_id: 1,
            parameter_names: ["region"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "region", title: "Region", type: "dropdown", enum_options: "us\neu" }] },
          },
        },
      }),
      makeWidget({
        id: 2,
        visualization: {
          id: 2,
          query_id: 11,
          type: "table",
          name: "Widget B",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 11,
            name: "q2",
            data_source_id: 1,
            parameter_names: ["region"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "region", title: "Region", type: "dropdown", enum_options: "us\napac" }] },
          },
        },
      }),
    ];

    const resolved = resolveDashboardParams(widgets);
    expect(resolved).toHaveLength(1);
    expect(resolved[0].conflict).toEqual({ widgetName: "Widget B", type: "dropdown", sameType: true });
  });

  it("records a conflict when a later widget defines the same token, same type, with a different query_id", () => {
    const widgets = [
      makeWidget({
        id: 1,
        visualization: {
          id: 1,
          query_id: 10,
          type: "table",
          name: "Widget A",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 10,
            name: "q1",
            data_source_id: 1,
            parameter_names: ["owner"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "owner", title: "Owner", type: "query", query_id: 20 }] },
          },
        },
      }),
      makeWidget({
        id: 2,
        visualization: {
          id: 2,
          query_id: 11,
          type: "table",
          name: "Widget B",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 11,
            name: "q2",
            data_source_id: 1,
            parameter_names: ["owner"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "owner", title: "Owner", type: "query", query_id: 21 }] },
          },
        },
      }),
    ];

    const resolved = resolveDashboardParams(widgets);
    expect(resolved).toHaveLength(1);
    expect(resolved[0].conflict).toEqual({ widgetName: "Widget B", type: "query", sameType: true });
  });

  it("does NOT flag a conflict when a later widget defines the same token with the same type and config, only a different title", () => {
    const widgets = [
      makeWidget({
        id: 1,
        visualization: {
          id: 1,
          query_id: 10,
          type: "table",
          name: "Widget A",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 10,
            name: "q1",
            data_source_id: 1,
            parameter_names: ["region"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "region", title: "Region", type: "dropdown", enum_options: "us\neu" }] },
          },
        },
      }),
      makeWidget({
        id: 2,
        visualization: {
          id: 2,
          query_id: 11,
          type: "table",
          name: "Widget B",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 11,
            name: "q2",
            data_source_id: 1,
            parameter_names: ["region"],
            verified: false,
            is_published: true,
            options: { parameters: [{ name: "region", title: "Region (other)", type: "dropdown", enum_options: "us\neu" }] },
          },
        },
      }),
    ];

    const resolved = resolveDashboardParams(widgets);
    expect(resolved).toHaveLength(1);
    expect(resolved[0].conflict).toBeUndefined();
  });

  it("a token referenced in SQL but defined nowhere resolves with no definition", () => {
    const widgets = [
      makeWidget({
        id: 1,
        visualization: {
          id: 1,
          query_id: 10,
          type: "table",
          name: "Widget A",
          options: {},
          created_at: "",
          updated_at: "",
          query: {
            id: 10,
            name: "q1",
            data_source_id: 1,
            parameter_names: ["orphan"],
            verified: false,
            is_published: true,
            options: {},
          },
        },
      }),
    ];

    const resolved = resolveDashboardParams(widgets);
    expect(resolved).toHaveLength(1);
    expect(resolved[0].name).toBe("orphan");
    expect(resolved[0].definition).toBeUndefined();
  });

  it("returns no entries when no widget references any parameter tokens", () => {
    const widgets = [
      makeWidget({
        id: 1,
        visualization: {
          id: 1,
          query_id: 10,
          type: "table",
          name: "Widget A",
          options: {},
          created_at: "",
          updated_at: "",
          query: { id: 10, name: "q1", data_source_id: 1, parameter_names: [], options: {}, verified: false, is_published: true },
        },
      }),
    ];

    expect(resolveDashboardParams(widgets)).toEqual([]);
  });
});

describe("URL round-trip (values → URL → parsed back)", () => {
  it("serializes scalars as plain strings and ranges/arrays via JSON.stringify, and parses back the same shapes", () => {
    const values = {
      region: "us",
      tags: ["a", "b"],
      window: { start: "2026-01-01", end: "2026-01-31" },
    };

    const search = serializeValuesToSearch(values);
    expect(search).toEqual({
      p_region: "us",
      p_tags: JSON.stringify(["a", "b"]),
      p_window: JSON.stringify({ start: "2026-01-01", end: "2026-01-31" }),
    });

    const params = new URLSearchParams(search);
    const parsed = parseValuesFromSearch(params);
    expect(parsed).toEqual(values);
  });

  it("skips a malformed p_ key instead of throwing", () => {
    const params = new URLSearchParams({ p_broken: "{not json", p_ok: "fine" });
    expect(() => parseValuesFromSearch(params)).not.toThrow();
    expect(parseValuesFromSearch(params)).toEqual({ ok: "fine" });
  });

  it("ignores non-p_-prefixed search params", () => {
    const params = new URLSearchParams({ tab: "widgets", p_region: "us" });
    expect(parseValuesFromSearch(params)).toEqual({ region: "us" });
  });

  it("omits empty-string scalar values from the URL", () => {
    expect(serializeValuesToSearch({ region: "" })).toEqual({});
  });
});
