import { describe, it, expect } from "vitest";
import { aiErrorMessage } from "./ai";

describe("aiErrorMessage", () => {
  it("appends the retry-after suffix for a rate_limited response", () => {
    expect(
      aiErrorMessage({ status: "rate_limited", message: "Too many requests.", retry_after_secs: 34 }),
    ).toBe("Too many requests. (try again in 34s)");
  });

  it("returns the message verbatim for every other status", () => {
    expect(aiErrorMessage({ status: "not_configured", message: "No provider." })).toBe(
      "No provider.",
    );
  });
});
