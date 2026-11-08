import { describe, it, expect } from "vitest";
import { vizRegistry, getViz } from "../index";

describe("visualizations/index side-effecting registration", () => {
  it("populates vizRegistry with the 4 implemented types", () => {
    expect(vizRegistry.map((v) => v.type)).toEqual(["TABLE", "CHART", "COUNTER", "FUNNEL"]);
  });

  it("resolves each real renderer type via getViz", () => {
    expect(getViz("TABLE")).toBeDefined();
    expect(getViz("CHART")).toBeDefined();
    expect(getViz("COUNTER")).toBeDefined();
    expect(getViz("FUNNEL")).toBeDefined();
    expect(getViz("table")).toBeDefined(); // case-insensitive
  });
});
