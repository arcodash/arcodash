import { describe, it, expect } from "vitest";
import { axisTick, formatCounter, formatNumeric, formatUsd } from "./formatNumber";

describe("formatNumeric", () => {
  it("groups a plain integer", () => {
    expect(formatNumeric(60000)).toBe("60,000");
  });

  it("groups a numeric string while preserving its own decimal precision", () => {
    expect(formatNumeric("33375.25")).toBe("33,375.25");
  });

  it("preserves a negative sign", () => {
    expect(formatNumeric("-1234.5")).toBe("-1,234.5");
    expect(formatNumeric(-1234)).toBe("-1,234");
  });

  it("preserves a signed negative zero", () => {
    expect(formatNumeric("-0.00")).toBe("-0.00");
  });

  it("does not round or pad the fractional part", () => {
    expect(formatNumeric("1000.123456")).toBe("1,000.123456");
  });

  it("passes through non-numeric-looking input unchanged rather than throwing", () => {
    expect(formatNumeric("")).toBe("");
    expect(formatNumeric("NaN")).toBe("NaN");
    expect(formatNumeric("1.2.3")).toBe("1.2.3");
  });

  it("handles a large NUMERIC value that would lose precision as a float", () => {
    expect(formatNumeric("123456789012345678.99")).toBe("123,456,789,012,345,678.99");
  });
});

describe("axisTick", () => {
  it("groups a numeric axis value the same way as the table", () => {
    expect(axisTick(60000)).toBe("60,000");
  });

  it("groups a numeric-looking string tick value", () => {
    expect(axisTick("2500")).toBe("2,500");
  });

  it("leaves a short category label untouched", () => {
    expect(axisTick("Jan")).toBe("Jan");
  });

  it("truncates a long category label to 14 characters with an ellipsis", () => {
    const result = axisTick("Lumen Camden Market Street Flagship");
    expect(result.length).toBe(14);
    expect(result.endsWith("…")).toBe(true);
  });

  it("renders a missing value as an empty string, not 'null'/'undefined'", () => {
    expect(axisTick(null)).toBe("");
    expect(axisTick(undefined)).toBe("");
  });

  it("does not treat a non-leading-digit string as numeric", () => {
    // "2026-01-01" starts with a digit and should still group; a string that doesn't
    // start with a digit (a store name, a status) must never be pushed through BigInt.
    expect(axisTick("region")).toBe("region");
  });
});

describe("formatCounter", () => {
  it("auto groups, fixed pads, grouping off", () => {
    expect(formatCounter(4222.9)).toBe("4,222.9");
    expect(formatCounter(4222.9, { decimals: 2 })).toBe("4,222.90");
    expect(formatCounter(4222.9, { decimals: 2, grouping: false })).toBe("4222.90");
    expect(formatCounter(4222.9, { grouping: false })).toBe("4222.9");
  });
  it("exact string path for huge NUMERIC", () => {
    expect(formatCounter("12345678901234567890.5", { decimals: 0 })).toBe("12,345,678,901,234,567,891");
  });
  it("negatives, null, and non-numeric", () => {
    expect(formatCounter(-1234.5)).toBe("-1,234.5");
    expect(formatCounter(null)).toBe("—");
    expect(formatCounter("abc")).toBe("abc");
    expect(formatCounter(NaN)).toBe("NaN");
    expect(formatCounter(Infinity)).toBe("Infinity");
    expect(formatCounter(true)).toBe("true");
  });
});

describe("formatUsd", () => {
  it.each([
    [null, "—"],
    [undefined, "—"],
    [0, "$0.00"],
    [0.004, "<$0.01"],
    [0.005, "$0.01"],
    [0.01, "$0.01"],
    [12.4, "$12.40"],
    [1234.5, "$1,234.50"],
  ])("formats %s as %s", (input, expected) => {
    expect(formatUsd(input)).toBe(expected);
  });
});
