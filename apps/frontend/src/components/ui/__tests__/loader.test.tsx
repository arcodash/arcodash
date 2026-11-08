import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { Loader, PageLoader, INLINE_POOL } from "@/components/ui/loader";

describe("Loader", () => {
  it.each(["pendulum", "radar", "bounce", "comet"] as const)(
    "renders the %s variant icon",
    (variant) => {
      const { container } = render(<Loader variant={variant} />);
      expect(container.querySelector("svg")).toBeInTheDocument();
    },
  );

  it("renders no label by default (icon-only)", () => {
    const { container } = render(<Loader variant="pendulum" />);
    expect(container.querySelector("span > span")).not.toBeInTheDocument();
  });

  it("renders the default 'Loading…' text when label is true", () => {
    render(<Loader variant="pendulum" label />);
    expect(screen.getByText("Loading…")).toBeInTheDocument();
  });

  it("renders a custom string label", () => {
    render(<Loader variant="pendulum" label="Fetching rows…" />);
    expect(screen.getByText("Fetching rows…")).toBeInTheDocument();
  });

  it("has role=status with a default aria-label of 'Loading'", () => {
    render(<Loader variant="pendulum" announce />);
    expect(screen.getByRole("status")).toHaveAttribute("aria-label", "Loading");
  });

  it("uses the custom string label as the aria-label", () => {
    render(<Loader variant="pendulum" label="Fetching rows…" announce />);
    expect(screen.getByRole("status")).toHaveAttribute("aria-label", "Fetching rows…");
  });

  it("never picks the flip variant when unmounted (excluded from inline pool)", () => {
    expect(INLINE_POOL).not.toContain("flip");
    // Render many random-variant mounts; none should produce the flip tile
    // (flip is the only variant that renders a <rect rx="11"> brand tile).
    for (let i = 0; i < 50; i++) {
      const { container, unmount } = render(<Loader />);
      expect(container.querySelector("rect")).not.toBeInTheDocument();
      unmount();
    }
  });
});

describe("PageLoader", () => {
  it("renders the given variant and line", () => {
    render(<PageLoader variant="radar" line="Custom line here." />);
    expect(screen.getByText("Custom line here.")).toBeInTheDocument();
  });

  it("has role=status on the wrapper", () => {
    render(<PageLoader variant="radar" line="Custom line here." />);
    expect(screen.getByRole("status")).toBeInTheDocument();
  });

  it("renders a default line for the flip variant", () => {
    render(<PageLoader variant="flip" />);
    expect(
      screen.getByText(
        (_, el) =>
          el?.tagName === "P" &&
          (el.textContent === "Flipping through your data." ||
            el.textContent === "Turning it over, one side at a time."),
      ),
    ).toBeInTheDocument();
  });
});
