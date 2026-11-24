import { describe, expect, it } from "vitest";
import { exploreSql, exploreState } from "./explore";

describe("exploreSql", () => {
  it("builds the five-line starter query", () => {
    expect(exploreSql(42, "Revenue by region")).toBe(
      `-- Explore of "Revenue by region" (#42).
-- query_42 is its cached result. This runs inside arcodash, not in your database.
SELECT *
FROM query_42
LIMIT 100`,
    );
  });

  it("escapes quotes and flattens newlines in the name", () => {
    const first = exploreSql(1, 'a "b"\nc').split("\n")[0];
    expect(first).toBe('-- Explore of "a \\"b\\" c" (#1).');
  });
});

describe("exploreState", () => {
  it("carries the data source and query identity", () => {
    const s = exploreState(42, "x", 9);
    expect(s.dataSourceId).toBe(9);
    expect(s.explore).toEqual({ queryId: 42, queryName: "x" });
    expect(s.sql).toContain("FROM query_42");
  });
});
