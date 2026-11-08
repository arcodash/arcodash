import { describe, it, expect, vi, afterEach } from "vitest";
import { copyToClipboard, matchesSearch } from "./utils";

const originalClipboard = navigator.clipboard;

afterEach(() => {
  Object.defineProperty(navigator, "clipboard", { value: originalClipboard, configurable: true });
});

describe("copyToClipboard", () => {
  it("resolves true and forwards the text when writeText resolves", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });

    await expect(copyToClipboard("hello")).resolves.toBe(true);
    expect(writeText).toHaveBeenCalledWith("hello");
  });

  it("resolves false when navigator.clipboard is undefined", async () => {
    Object.defineProperty(navigator, "clipboard", { value: undefined, configurable: true });

    await expect(copyToClipboard("hello")).resolves.toBe(false);
  });

  it("resolves false when writeText rejects", async () => {
    const writeText = vi.fn().mockRejectedValue(new Error("permission denied"));
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });

    await expect(copyToClipboard("hello")).resolves.toBe(false);
  });
});

describe("matchesSearch", () => {
  it("trims, ignores case, matches any field, and blank matches all", () => {
    expect(matchesSearch("  ALI ", "Bob", "alice@x.io")).toBe(true);
    expect(matchesSearch("zed", "Bob", "alice@x.io")).toBe(false);
    expect(matchesSearch("   ", "anything")).toBe(true);
  });
});
