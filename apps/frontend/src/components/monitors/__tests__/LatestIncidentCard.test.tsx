import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { LatestIncidentCard } from "../LatestIncidentCard";
import { splitExplanation } from "@/lib/monitors";

const obs = (explanation: string, run_id: number | null = 7) => ({
  observed_at: "2026-01-01T00:00:00.000Z",
  value: 913,
  state: "triggered",
  explanation,
  run_id,
});

describe("splitExplanation", () => {
  it("splits stats and AI part", () => {
    expect(splitExplanation("stats\n\nAI analysis (confidence: high): because X")).toEqual({
      stats: "stats",
      ai: "because X",
      confidence: "high",
    });
  });
  it("keeps everything in stats without a marker or with a bad confidence", () => {
    expect(splitExplanation("stats only")).toEqual({ stats: "stats only" });
    const bad = "s\n\nAI analysis (confidence: wild): x";
    expect(splitExplanation(bad)).toEqual({ stats: bad });
  });
});

describe("LatestIncidentCard", () => {
  it("renders stats and the AI panel separately", () => {
    render(<LatestIncidentCard observation={obs("backlog is 913\n\nAI analysis (confidence: low): <b>x</b>")} onViewRun={() => {}} />);
    expect(screen.getByText("backlog is 913")).toBeInTheDocument();
    const ai = screen.getByTestId("latest-incident-ai");
    expect(ai).toHaveTextContent("AI analysis · low confidence");
    expect(ai).toHaveTextContent("<b>x</b>");
    expect(ai).not.toHaveTextContent("backlog is 913");
  });

  it("omits the AI panel without a marker", () => {
    render(<LatestIncidentCard observation={obs("backlog is 913")} onViewRun={() => {}} />);
    expect(screen.queryByTestId("latest-incident-ai")).not.toBeInTheDocument();
  });

  it("View run reports the run id; no link without one", async () => {
    const onViewRun = vi.fn();
    const { rerender } = render(<LatestIncidentCard observation={obs("s")} onViewRun={onViewRun} />);
    await userEvent.click(screen.getByRole("button", { name: "View run" }));
    expect(onViewRun).toHaveBeenCalledWith(7);
    rerender(<LatestIncidentCard observation={obs("s", null)} onViewRun={onViewRun} />);
    expect(screen.queryByRole("button", { name: "View run" })).not.toBeInTheDocument();
  });
});
