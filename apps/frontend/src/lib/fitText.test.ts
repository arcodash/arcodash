import { describe, expect, it } from "vitest";
import { fitFontSize } from "./fitText";

describe("fitFontSize", () => {
  it("bounds the size between min and max", () => {
    expect(fitFontSize("913", 1.875)).toBe("clamp(0.875rem, calc(100cqw / 1.95), 1.875rem)");
  });

  it("divides the container by a wider divisor for longer text", () => {
    expect(fitFontSize("1,598,234,000", 1.875)).toContain("100cqw / 8.45");
  });

  it("honours a custom min and treats empty text as one character", () => {
    expect(fitFontSize("", 3, 1)).toBe("clamp(1rem, calc(100cqw / 0.65), 3rem)");
  });
});
