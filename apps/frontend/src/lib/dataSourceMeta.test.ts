import { describe, it, expect } from "vitest";
import { compareDataSourceTypes, POPULARITY_ORDER } from "./dataSourceMeta";

describe("compareDataSourceTypes", () => {
  it("ranks POPULARITY_ORDER entries in that exact order, before anything else", () => {
    const shuffled = [
      { type: "athena", name: "Amazon Athena" },
      { type: "pg", name: "PostgreSQL" },
      { type: "snowflake", name: "Snowflake" },
      { type: "mysql", name: "MySQL" },
    ];
    const sorted = [...shuffled].sort(compareDataSourceTypes);
    expect(sorted.map((t) => t.type)).toEqual(["pg", "mysql", "snowflake", "athena"]);
  });

  it("falls back to alphabetical-by-name for types absent from POPULARITY_ORDER", () => {
    const shuffled = [
      { type: "influxdb", name: "InfluxDB" },
      { type: "es", name: "Elasticsearch" },
      { type: "prometheus", name: "Prometheus" },
    ];
    const sorted = [...shuffled].sort(compareDataSourceTypes);
    expect(sorted.map((t) => t.name)).toEqual(["Elasticsearch", "InfluxDB", "Prometheus"]);
  });

  it("puts every POPULARITY_ORDER entry ahead of every unranked one", () => {
    const shuffled = [
      { type: "influxdb", name: "InfluxDB" },
      { type: "redshift", name: "Amazon Redshift" },
    ];
    const sorted = [...shuffled].sort(compareDataSourceTypes);
    expect(sorted.map((t) => t.type)).toEqual(["redshift", "influxdb"]);
  });

  it("POPULARITY_ORDER starts with the most common databases", () => {
    expect(POPULARITY_ORDER.slice(0, 3)).toEqual(["pg", "mysql", "bigquery"]);
  });
});
