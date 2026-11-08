import { describe, it, expect } from "vitest";
import { resolveWidgetParams } from "@/components/dashboard/WidgetCard";

const TOKENS = ["region", "d"];

describe("resolveWidgetParams", () => {
  it("returns undefined when the widget has no parameter tokens", () => {
    expect(resolveWidgetParams(undefined, { region: "us" })).toBeUndefined();
  });

  it("returns undefined when there are no dashboard params at all", () => {
    expect(resolveWidgetParams(TOKENS, undefined)).toBeUndefined();
  });

  it("returns undefined when the token list is empty (nothing to gate on)", () => {
    expect(resolveWidgetParams([], { region: "us" })).toBeUndefined();
  });

  it("returns undefined when a required token is entirely missing", () => {
    expect(resolveWidgetParams(TOKENS, { region: "us" })).toBeUndefined();
  });

  it("returns undefined when a range value has only start, not end (half-typed)", () => {
    expect(
      resolveWidgetParams(TOKENS, { region: "us", d: { start: "2026-01-01" } }),
    ).toBeUndefined();
  });

  it("returns undefined when a scalar is an empty string", () => {
    expect(
      resolveWidgetParams(TOKENS, { region: "", d: { start: "2026-01-01", end: "2026-01-31" } }),
    ).toBeUndefined();
  });

  it("returns undefined when an array value is empty", () => {
    expect(resolveWidgetParams(["tags"], { tags: [] })).toBeUndefined();
  });

  it("treats the number 0 as a complete scalar value", () => {
    expect(resolveWidgetParams(["n"], { n: 0 })).toEqual({ n: 0 });
  });

  it("narrows to only the tokens this widget uses, ignoring unrelated dashboard params", () => {
    const result = resolveWidgetParams(["region"], {
      region: "us",
      unrelated_range: { start: "", end: "" },
      unrelated_scalar: "",
    });
    expect(result).toEqual({ region: "us" });
  });

  it("returns the full narrowed set once every token used is complete", () => {
    const result = resolveWidgetParams(TOKENS, {
      region: "us",
      d: { start: "2026-01-01", end: "2026-01-31" },
    });
    expect(result).toEqual({
      region: "us",
      d: { start: "2026-01-01", end: "2026-01-31" },
    });
  });
});
