import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { ErrorBoundary } from "@/components/ErrorBoundary";

function Bomb({ throws }: { throws: boolean }) {
  if (throws) throw new Error("boom: SELECT * FROM secrets");
  return <p>ok</p>;
}

describe("ErrorBoundary", () => {
  afterEach(() => vi.restoreAllMocks());

  it("renders the fallback when a child throws", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <ErrorBoundary>
        <Bomb throws />
      </ErrorBoundary>,
    );
    expect(screen.getByText("Something went wrong rendering this.")).toBeInTheDocument();
  });

  it("clears the error and renders children again once a resetKeys member changes", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { rerender } = render(
      <ErrorBoundary resetKeys={["a"]}>
        <Bomb throws />
      </ErrorBoundary>,
    );
    expect(screen.getByText("Something went wrong rendering this.")).toBeInTheDocument();

    rerender(
      <ErrorBoundary resetKeys={["b"]}>
        <Bomb throws={false} />
      </ErrorBoundary>,
    );
    expect(screen.getByText("ok")).toBeInTheDocument();
  });

  it("keeps showing the fallback when resetKeys is re-passed unchanged", () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { rerender } = render(
      <ErrorBoundary resetKeys={["a"]}>
        <Bomb throws />
      </ErrorBoundary>,
    );
    expect(screen.getByText("Something went wrong rendering this.")).toBeInTheDocument();

    // Fresh array literal, same values — must not be treated as a change.
    rerender(
      <ErrorBoundary resetKeys={["a"]}>
        <Bomb throws={false} />
      </ErrorBoundary>,
    );
    expect(screen.getByText("Something went wrong rendering this.")).toBeInTheDocument();
  });
});
