import { describe, it, expect } from "vitest";
import { vizRegistry, registerViz, getViz, type VizDefinition } from "../registry";

function makeDef(type: string): VizDefinition {
  return {
    type,
    name: type,
    Renderer: () => null,
    Editor: () => null,
    defaultOptions: {},
  };
}

describe("registry", () => {
  it("registerViz adds an entry to vizRegistry", () => {
    const before = vizRegistry.length;
    registerViz(makeDef("WIDGET"));
    expect(vizRegistry.length).toBe(before + 1);
    expect(vizRegistry.at(-1)?.type).toBe("WIDGET");
  });

  it("getViz finds by type case-insensitively", () => {
    registerViz(makeDef("FOOBAR"));
    expect(getViz("FOOBAR")?.type).toBe("FOOBAR");
    expect(getViz("foobar")?.type).toBe("FOOBAR");
    expect(getViz("FooBar")?.type).toBe("FOOBAR");
  });

  it("getViz returns undefined for an unknown type", () => {
    expect(getViz("DOES_NOT_EXIST")).toBeUndefined();
  });
});
