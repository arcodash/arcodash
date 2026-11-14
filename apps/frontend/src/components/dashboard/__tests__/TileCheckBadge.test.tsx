import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { TileCheckBadge } from "../TileCheckBadge";
import type { CheckSummary } from "@/types/api";

function renderBadge(summary: CheckSummary | null, canAddMonitor = true) {
  return render(
    <MemoryRouter>
      <TileCheckBadge summary={summary} queryId={42} canAddMonitor={canAddMonitor} />
    </MemoryRouter>,
  );
}

const s = (total: number, ok: number, triggered: number, unevaluable: number): CheckSummary => ({
  total,
  ok,
  triggered,
  unevaluable,
});

describe("TileCheckBadge", () => {
  it.each([
    ["failing", s(2, 1, 1, 0), "1 check failing"],
    ["failing plural", s(3, 0, 2, 0), "2 checks failing"],
    ["not run", s(3, 0, 0, 2), "2 checks not run"],
    ["not run singular", s(1, 0, 0, 1), "1 check not run"],
    ["pass", s(2, 2, 0, 0), "2 checks pass"],
    ["pass singular", s(1, 1, 0, 0), "1 check passes"],
    ["pending", s(1, 0, 0, 0), "1 check pending"],
    ["pending plural", s(3, 0, 0, 0), "3 checks pending"],
  ])("%s links to the filtered monitors list", (_n, summary, text) => {
    renderBadge(summary);
    expect(screen.getByRole("link", { name: text })).toHaveAttribute("href", "/monitors?query_id=42");
  });

  it("offers adding a check when there are none", () => {
    renderBadge(null);
    expect(screen.getByRole("link", { name: "No checks · add one" })).toHaveAttribute(
      "href",
      "/monitors/new?query_id=42",
    );
  });

  it("shows a plain label when the viewer cannot add a monitor", () => {
    renderBadge(null, false);
    expect(screen.getByText("No checks")).toBeInTheDocument();
    expect(screen.queryByRole("link")).not.toBeInTheDocument();
  });
});
