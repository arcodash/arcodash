import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { ExecuteStatusBanner } from "../ExecuteStatusBanner";

const BASE_PROPS = {
  isPending: false,
  isStarted: false,
  isFailure: false,
  isCancelled: false,
  gaveUp: false,
  error: null,
};

describe("ExecuteStatusBanner — cancelled state", () => {
  it("renders the job's real error (e.g. a paused-data-source skip reason) instead of the generic copy", () => {
    render(
      <ExecuteStatusBanner
        {...BASE_PROPS}
        isCancelled
        error="this data source is paused: maintenance window"
      />,
    );
    expect(screen.getByText("this data source is paused: maintenance window")).toBeInTheDocument();
    expect(screen.queryByText(/query stopped/i)).not.toBeInTheDocument();
    expect(screen.queryByText(/source database may keep running/i)).not.toBeInTheDocument();
  });

  it("falls back to the generic copy when a cancelled job carries no error", () => {
    render(<ExecuteStatusBanner {...BASE_PROPS} isCancelled error={null} />);
    expect(screen.getByText(/query stopped/i)).toBeInTheDocument();
    expect(screen.getByText(/source database may keep running/i)).toBeInTheDocument();
  });

  it("falls back to the generic copy when a cancelled job's error is an empty string", () => {
    render(<ExecuteStatusBanner {...BASE_PROPS} isCancelled error="" />);
    expect(screen.getByText(/query stopped/i)).toBeInTheDocument();
  });
});
