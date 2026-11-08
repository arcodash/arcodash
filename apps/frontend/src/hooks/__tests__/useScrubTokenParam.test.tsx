import { describe, it, expect } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, useSearchParams } from "react-router";
import { useScrubTokenParam } from "../useScrubTokenParam";

function Probe() {
  const token = useScrubTokenParam("token");
  const [params] = useSearchParams();
  return (
    <div>
      <span data-testid="token">{token}</span>
      <span data-testid="search">{params.toString()}</span>
    </div>
  );
}

describe("useScrubTokenParam", () => {
  it("captures the token once, then scrubs it from the URL without disturbing other params", async () => {
    render(
      <MemoryRouter initialEntries={["/invite?token=secret123&other=1"]}>
        <Probe />
      </MemoryRouter>,
    );
    expect(screen.getByTestId("token")).toHaveTextContent("secret123");

    await waitFor(() => expect(screen.getByTestId("search")).not.toHaveTextContent(/token/));
    expect(screen.getByTestId("search")).toHaveTextContent("other=1");
    // Still returns the originally captured value — callers need the token even after the scrub.
    expect(screen.getByTestId("token")).toHaveTextContent("secret123");
  });

  it("returns an empty string and touches nothing when the param was never present", () => {
    render(
      <MemoryRouter initialEntries={["/invite"]}>
        <Probe />
      </MemoryRouter>,
    );
    expect(screen.getByTestId("token")).toHaveTextContent("");
    expect(screen.getByTestId("search")).toHaveTextContent("");
  });
});
