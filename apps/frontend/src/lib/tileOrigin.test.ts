import { describe, it, expect } from "vitest";
import { parseTileOrigin, resolveTileLabel } from "./tileOrigin";
import type { Dashboard } from "@/types/api";

const p = (s: string) => new URLSearchParams(s);

describe("parseTileOrigin", () => {
  it("parses valid params", () => {
    expect(parseTileOrigin(p("query_id=7&from=tile&dashboard=sales-q3&widget=7"))).toEqual({
      dashboardSlug: "sales-q3",
      widgetId: 7,
    });
  });

  it("accepts non-Latin dashboard slugs", () => {
    expect(parseTileOrigin(p("query_id=7&from=tile&dashboard=हिंदी-1a2b3c&widget=7"))).toEqual({
      dashboardSlug: "हिंदी-1a2b3c",
      widgetId: 7,
    });
  });

  it.each([
    "query_id=7&from=tile&dashboard=a.b&widget=7",
    "query_id=7&from=tile&dashboard=a%2Fb&widget=7",
    "query_id=7&dashboard=sales-q3&widget=7",
    "query_id=7&from=tile&dashboard=sales-q3&widget=abc",
    "query_id=7&from=tile&dashboard=sales-q3&widget=0",
    "query_id=7&from=tile&dashboard=../x&widget=7",
    "query_id=7&from=tile&dashboard=&widget=7",
    "from=tile&dashboard=sales-q3&widget=7",
  ])("rejects %s", (s) => {
    expect(parseTileOrigin(p(s))).toBeNull();
  });
});

describe("resolveTileLabel", () => {
  const origin = { dashboardSlug: "sales-q3", widgetId: 3 };
  const dash = {
    name: "Sales Q3",
    widgets: [{ id: 3, visualization: { name: "Revenue", query_id: 7, query: { id: 7 } } }],
  } as unknown as Dashboard;

  it("returns labels on a match", () => {
    expect(resolveTileLabel(dash, origin, 7)).toEqual({ tileTitle: "Revenue", dashboardName: "Sales Q3" });
  });
  it("returns null when the widget is missing or the query differs", () => {
    expect(resolveTileLabel(dash, { ...origin, widgetId: 9 }, 7)).toBeNull();
    expect(resolveTileLabel(dash, origin, 8)).toBeNull();
    expect(resolveTileLabel(undefined, origin, 7)).toBeNull();
  });
});
