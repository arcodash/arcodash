import { describe, it, expect } from "vitest";
import { safeDestination } from "@/pages/LoginPage";

describe("safeDestination", () => {
  it("allows a same-site path", () => {
    expect(safeDestination("/dashboards")).toBe("/dashboards");
  });

  it("rejects a protocol-relative host", () => {
    expect(safeDestination("//evil.com")).toBe("/queries");
  });

  it("rejects an absolute URL", () => {
    expect(safeDestination("https://evil.com")).toBe("/queries");
  });

  it("falls back to default when undefined", () => {
    expect(safeDestination(undefined)).toBe("/queries");
  });
});
