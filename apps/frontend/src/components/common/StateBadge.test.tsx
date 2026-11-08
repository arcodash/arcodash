import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { StateBadge } from "./StateBadge";

describe("StateBadge", () => {
  it("renders Alerting for the triggered state", () => {
    render(<StateBadge state="triggered" />);
    expect(screen.getByText("Alerting")).toBeInTheDocument();
    expect(screen.queryByText("Triggered")).not.toBeInTheDocument();
  });

  it("renders OK for the ok state", () => {
    render(<StateBadge state="ok" />);
    expect(screen.getByText("OK")).toBeInTheDocument();
  });

  it("renders a distinct Can't evaluate badge for the unevaluable state", () => {
    render(<StateBadge state="unevaluable" />);
    const badge = screen.getByText("Can't evaluate");
    expect(badge).toBeInTheDocument();
    // Must not fall through to the generic "Not checked yet" label.
    expect(screen.queryByText("Not checked yet")).not.toBeInTheDocument();
  });

  it("falls back to Not checked yet for a never-checked state", () => {
    render(<StateBadge state="unknown" />);
    expect(screen.getByText("Not checked yet")).toBeInTheDocument();
  });

  it("renders Check failed for a failed check on a non-triggered state", () => {
    render(<StateBadge state="ok" checkFailed />);
    expect(screen.getByText("Check failed")).toBeInTheDocument();
    expect(screen.queryByText("OK")).not.toBeInTheDocument();
  });

  it("keeps Alerting when triggered even if the check failed", () => {
    render(<StateBadge state="triggered" checkFailed />);
    expect(screen.getByText("Alerting")).toBeInTheDocument();
    expect(screen.queryByText("Check failed")).not.toBeInTheDocument();
  });

  it("is unchanged when checkFailed is not passed", () => {
    render(<StateBadge state="ok" />);
    expect(screen.getByText("OK")).toBeInTheDocument();
  });
});
