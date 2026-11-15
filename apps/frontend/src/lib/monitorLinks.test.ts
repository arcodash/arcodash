import { describe, it, expect } from "vitest";
import { newMonitorHref } from "./monitorLinks";

describe("newMonitorHref", () => {
  it("links by query alone without an origin", () => {
    expect(newMonitorHref(42)).toBe("/monitors/new?query_id=42");
  });

  it("carries the tile origin", () => {
    expect(newMonitorHref(42, { dashboardSlug: "sales-q3", widgetId: 7 })).toBe(
      "/monitors/new?query_id=42&from=tile&dashboard=sales-q3&widget=7",
    );
  });

  it("percent-encodes the slug", () => {
    expect(newMonitorHref(42, { dashboardSlug: "my board", widgetId: 7 })).toContain("dashboard=my+board");
  });
});
