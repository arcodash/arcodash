import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  SuggestionCard,
  evaluatesTrue,
  createSuggestedCheck,
  PartialCheckError,
  type CreateStatus,
  type PartialCheck,
} from "../SuggestionCard";
import type { MonitorSuggestion } from "@/types/api";
import type * as ApiModule from "@/lib/api";
import type * as JobsModule from "@/lib/jobs";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      executeAdHoc: vi.fn(),
      getJob: vi.fn(),
      createQuery: vi.fn(),
      publishQuery: vi.fn(),
      createMonitor: vi.fn(),
      createMonitorSubscription: vi.fn(),
    },
  };
});

vi.mock("@/lib/jobs", async (importOriginal) => {
  const actual = await importOriginal<typeof JobsModule>();
  return { ...actual, usePollJob: vi.fn(actual.usePollJob) };
});

import { api } from "@/lib/api";
import { usePollJob } from "@/lib/jobs";

const mockedApi = api as unknown as {
  executeAdHoc: ReturnType<typeof vi.fn>;
  createQuery: ReturnType<typeof vi.fn>;
  publishQuery: ReturnType<typeof vi.fn>;
  createMonitor: ReturnType<typeof vi.fn>;
  createMonitorSubscription: ReturnType<typeof vi.fn>;
};

const mockedUsePollJob = usePollJob as unknown as ReturnType<typeof vi.fn>;

/** A terminal `usePollJob` result; tests override only the fields they care about. */
function pollResult(overrides: Partial<ReturnType<typeof usePollJob>>): ReturnType<typeof usePollJob> {
  return {
    job: null,
    status: null,
    queryResultId: null,
    error: null,
    isDone: true,
    isPending: false,
    isStarted: false,
    isSuccess: false,
    isFailure: false,
    isCancelled: false,
    isPollError: false,
    gaveUp: false,
    isLoading: false,
    queryState: {} as ReturnType<typeof usePollJob>["queryState"],
    ...overrides,
  };
}

function suggestion(overrides: Partial<MonitorSuggestion> = {}): MonitorSuggestion {
  return {
    table: "orders",
    kind: "freshness",
    title: "Orders stop arriving",
    rationale: "Alerts if orders goes stale.",
    sql: "SELECT EXTRACT(EPOCH FROM (now() - MAX(created_at)))/3600 AS hours_since_update FROM orders",
    column: "hours_since_update",
    target_column: "created_at",
    op: ">",
    value: 24,
    guessed_threshold: true,
    ...overrides,
  };
}

function renderCard(props: Partial<Parameters<typeof SuggestionCard>[0]> = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onCreateStatusChange = vi.fn();
  const onTestFailedChange = vi.fn();
  const defaultProps = {
    dataSourceId: 1,
    suggestion: suggestion(),
    op: ">",
    value: "24",
    monitorKind: "threshold" as const,
    onMonitorKindChange: vi.fn(),
    onOpChange: vi.fn(),
    onValueChange: vi.fn(),
    selected: false,
    onSelectedChange: vi.fn(),
    createStatus: { kind: "idle" } as CreateStatus,
    onCreateStatusChange,
    onTestFailedChange,
    onDismiss: vi.fn(),
  };
  const merged = { ...defaultProps, ...props };
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <SuggestionCard {...merged} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return { onCreateStatusChange, onTestFailedChange };
}

describe("evaluatesTrue", () => {
  it.each([
    [">", 5, 4, true],
    [">", 4, 4, false],
    [">", 3, 4, false],
    ["<", 3, 4, true],
    ["<", 4, 4, false],
    ["<", 5, 4, false],
    [">=", 4, 4, true],
    [">=", 3, 4, false],
    ["<=", 4, 4, true],
    ["<=", 5, 4, false],
    ["==", 4, 4, true],
    ["==", 5, 4, false],
    ["!=", 5, 4, true],
    ["!=", 4, 4, false],
  ] as const)("%s: %d vs %d -> %s", (op, value, threshold, expected) => {
    expect(evaluatesTrue(value, op, threshold)).toBe(expected);
  });

  it("returns false for an unrecognized operator", () => {
    expect(evaluatesTrue(5, "~=", 4)).toBe(false);
  });
});

describe("SuggestionCard", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockedUsePollJob.mockReturnValue(pollResult({ isDone: false }));
  });

  it.each([
    ["freshness", "created_at", "Hours since the newest orders.created_at"],
    ["uniqueness", "email", "Duplicate values in orders.email"],
    ["null_rate", "email", "% of orders.email that is null"],
    ["row_count", "", "Rows in orders"],
    ["freshness", "", "Hours since the newest row in orders"],
    ["uniqueness", "", "Duplicate rows in orders"],
    ["null_rate", "", "% null in orders"],
  ] as const)("Measures field for %s / %j reads %j, never the result alias", (kind, target_column, text) => {
    renderCard({ suggestion: suggestion({ kind, target_column }) });
    const field = screen.getByText("Measures").parentElement!;
    expect(field).toHaveTextContent(text);
    expect(field).not.toHaveTextContent("hours_since_update");
    expect(screen.queryByText("Column")).not.toBeInTheDocument();
  });

  it("shows the guessed-threshold hint only when guessed_threshold is true", () => {
    renderCard({ suggestion: suggestion({ guessed_threshold: true }) });
    expect(screen.getByText("Adjust to fit your data.")).toBeInTheDocument();
  });

  it("does not show the guessed-threshold hint when guessed_threshold is false", () => {
    renderCard({ suggestion: suggestion({ guessed_threshold: false }) });
    expect(screen.queryByText("Adjust to fit your data.")).not.toBeInTheDocument();
  });

  it("keeps the SQL collapsed by default and Create check enabled without opening it", () => {
    renderCard();
    const details = screen.getByText("Show SQL").closest("details");
    expect(details).not.toHaveAttribute("open");
    expect(screen.getByRole("button", { name: "Create check" })).not.toBeDisabled();
  });

  it("Test success renders a 'No alert' verdict with the observed value when the value doesn't trip the condition", async () => {
    mockedApi.executeAdHoc.mockResolvedValue({
      query_result: { data: { rows: [{ hours_since_update: 3 }] } },
    });
    renderCard({ op: ">", value: "24" });

    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    expect(await screen.findByRole("status")).toHaveTextContent(
      "No alert. Hours since the newest created_at is 3. This check alerts when it is greater than 24.",
    );
    expect(screen.queryByText(/Would alert now/)).not.toBeInTheDocument();
  });

  it("Test success renders a 'Would alert now' verdict when the value already trips the condition", async () => {
    mockedApi.executeAdHoc.mockResolvedValue({
      query_result: { data: { rows: [{ hours_since_update: 48 }] } },
    });
    renderCard({ op: ">", value: "24" });

    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    expect(await screen.findByRole("status")).toHaveTextContent(
      "Would alert now. Hours since the newest created_at is 48. This check alerts when it is greater than 24.",
    );
  });

  it("Test success with a blank threshold asks for one instead of showing a false 'No alert' verdict", async () => {
    mockedApi.executeAdHoc.mockResolvedValue({
      query_result: { data: { rows: [{ hours_since_update: 3 }] } },
    });
    renderCard({ op: "<", value: "" });

    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    expect(await screen.findByRole("status")).toHaveTextContent(
      "Hours since the newest created_at is 3. Set a threshold above to see whether this check would alert.",
    );
    expect(screen.queryByText(/No alert\./)).not.toBeInTheDocument();
    expect(screen.queryByText(/less than 0/)).not.toBeInTheDocument();
  });

  it("marks a non-numeric threshold invalid and points the input at the error", () => {
    renderCard({ op: ">", value: "abc" });

    const input = screen.getByLabelText("Value");
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input).toHaveAccessibleDescription("Enter a number");
  });

  it("Test success against zero rows renders the no-first-row verdict, not a generic failure", async () => {
    mockedApi.executeAdHoc.mockResolvedValue({
      query_result: { data: { rows: [] } },
    });
    renderCard();

    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    expect(
      await screen.findByText(/Can't evaluate\. The query returned no rows/),
    ).toBeInTheDocument();
  });

  it("Test failure shows the DB error verbatim and disables Create until a re-test passes", async () => {
    mockedApi.executeAdHoc.mockRejectedValue(new Error("relation \"orders\" does not exist"));
    renderCard();

    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    expect(await screen.findByText(/Can't evaluate\./)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create check" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("reports a failed test to the parent so a box checked before the failure gets unselected", async () => {
    mockedApi.executeAdHoc.mockRejectedValue(new Error("boom"));
    const { onTestFailedChange } = renderCard({ selected: true });

    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    await waitFor(() => expect(onTestFailedChange).toHaveBeenLastCalledWith(true));
  });

  it("a poll timeout (gaveUp) does not block Create — an unknown outcome isn't a failure", async () => {
    mockedApi.executeAdHoc.mockResolvedValue({ job: { id: "job-1" } });
    mockedUsePollJob.mockReturnValue(
      pollResult({ gaveUp: true, error: "This run hasn't reported back. It may still be going — reload to check." }),
    );

    renderCard();
    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    await waitFor(() =>
      expect(screen.getByText(/hasn't reported back/i)).toBeInTheDocument(),
    );
    expect(screen.getByRole("button", { name: "Create check" })).not.toBeDisabled();
  });

  it("a real job failure still blocks Create, unlike gaveUp", async () => {
    mockedApi.executeAdHoc.mockResolvedValue({ job: { id: "job-1" } });
    mockedUsePollJob.mockReturnValue(pollResult({ isFailure: true, error: "worker crashed" }));

    renderCard();
    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    await waitFor(() => expect(screen.getByText(/worker crashed/i)).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Create check" })).toBeDisabled();
  });

  it("a test job that succeeds with no result id shows an unknown outcome, not silence", async () => {
    mockedApi.executeAdHoc.mockResolvedValue({ job: { id: "job-1" } });
    mockedUsePollJob.mockReturnValue(pollResult({ isSuccess: true, queryResultId: null }));

    renderCard();
    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    expect(await screen.findByText(/finished without returning a result/i)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create check" })).not.toBeDisabled();
  });

  it("blocks Create when the value is blank, non-numeric, or non-finite", () => {
    renderCard({ value: "" });
    expect(screen.getByRole("button", { name: "Create check" })).toBeDisabled();
  });

  it("fires the four-call create chain in order", async () => {
    mockedApi.createQuery.mockResolvedValue({ id: 10, working_version: 0 });
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockResolvedValue({ id: 20 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    const { onCreateStatusChange } = renderCard();
    await userEvent.click(screen.getByRole("button", { name: "Create check" }));

    await waitFor(() =>
      expect(onCreateStatusChange).toHaveBeenCalledWith({
        kind: "created",
        monitorId: 20,
        subscribed: true,
      }),
    );

    expect(mockedApi.createQuery).toHaveBeenCalledBefore(mockedApi.publishQuery as never);
    expect(mockedApi.publishQuery).toHaveBeenCalledBefore(mockedApi.createMonitor as never);
    expect(mockedApi.createMonitor).toHaveBeenCalledBefore(
      mockedApi.createMonitorSubscription as never,
    );
    expect(mockedApi.publishQuery).toHaveBeenCalledWith(10, 0);
    expect(mockedApi.createMonitor).toHaveBeenCalledWith(
      expect.objectContaining({
        query_id: 10,
        options: { column: "hours_since_update", op: ">", value: 24 },
      }),
    );
  });

  it("reports Created with a quiet note when the subscription call fails", async () => {
    mockedApi.createQuery.mockResolvedValue({ id: 10, working_version: 0 });
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockResolvedValue({ id: 20 });
    mockedApi.createMonitorSubscription.mockRejectedValue(new Error("subscribe failed"));

    const { onCreateStatusChange } = renderCard();
    await userEvent.click(screen.getByRole("button", { name: "Create check" }));

    await waitFor(() =>
      expect(onCreateStatusChange).toHaveBeenCalledWith({
        kind: "created",
        monitorId: 20,
        subscribed: false,
      }),
    );
  });

  it("reports Failed with Retry when publish fails", async () => {
    mockedApi.createQuery.mockResolvedValue({ id: 10, working_version: 0 });
    mockedApi.publishQuery.mockRejectedValue(new Error("conflict"));

    const { onCreateStatusChange } = renderCard();
    await userEvent.click(screen.getByRole("button", { name: "Create check" }));

    await waitFor(() =>
      expect(onCreateStatusChange).toHaveBeenCalledWith(
        expect.objectContaining({ kind: "failed" }),
      ),
    );
    expect(mockedApi.createMonitor).not.toHaveBeenCalled();
  });
});

describe("createSuggestedCheck", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("tags the created query 'data quality' — the query-tags feature's hook for AI-generated checks", async () => {
    mockedApi.createQuery.mockResolvedValue({ id: 10, working_version: 0 });
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockResolvedValue({ id: 20 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    await createSuggestedCheck(1, suggestion(), ">", 24);

    expect(mockedApi.createQuery).toHaveBeenCalledWith(
      expect.objectContaining({ tags: ["data quality"] }),
    );
  });

  it("a retry after a failed monitor create reuses the query it already made", async () => {
    mockedApi.createQuery.mockResolvedValue({ id: 10, working_version: 0 });
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockRejectedValueOnce(new Error("boom")).mockResolvedValueOnce({ id: 20 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    const s = suggestion();
    let resume: PartialCheck = {};
    await createSuggestedCheck(1, s, ">", 24).catch((err: unknown) => {
      expect(err).toBeInstanceOf(PartialCheckError);
      resume = (err as PartialCheckError).partial;
    });

    await createSuggestedCheck(1, s, ">", 24, resume);

    expect(mockedApi.createQuery).toHaveBeenCalledTimes(1);
    expect(mockedApi.createMonitor).toHaveBeenCalledTimes(2);
  });

  it("a retry after a failed publish does not create a second query", async () => {
    mockedApi.createQuery.mockResolvedValue({ id: 10, working_version: 0 });
    mockedApi.publishQuery.mockRejectedValueOnce(new Error("conflict")).mockResolvedValueOnce({});
    mockedApi.createMonitor.mockResolvedValue({ id: 20 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    const s = suggestion();
    let resume: PartialCheck = {};
    await createSuggestedCheck(1, s, ">", 24).catch((err: unknown) => {
      resume = (err as PartialCheckError).partial;
    });
    expect(resume).toEqual({ queryId: 10, workingVersion: 0, monitorId: undefined });

    await createSuggestedCheck(1, s, ">", 24, resume);

    expect(mockedApi.createQuery).toHaveBeenCalledTimes(1);
    expect(mockedApi.publishQuery).toHaveBeenCalledTimes(2);
  });

  it("a resumed create does not re-publish an already-published query", async () => {
    mockedApi.createMonitor.mockResolvedValue({ id: 20 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    await createSuggestedCheck(1, suggestion(), ">", 24, { queryId: 10 });

    expect(mockedApi.createQuery).not.toHaveBeenCalled();
    expect(mockedApi.publishQuery).not.toHaveBeenCalled();
    expect(mockedApi.createMonitor).toHaveBeenCalledWith(expect.objectContaining({ query_id: 10 }));
  });

  it("a fully successful create still reports the monitor id and subscription state", async () => {
    mockedApi.createQuery.mockResolvedValue({ id: 10, working_version: 0 });
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockResolvedValue({ id: 20 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    const result = await createSuggestedCheck(1, suggestion(), ">", 24);

    expect(result).toEqual({ queryId: 10, monitorId: 20, subscribed: true });
  });
});

describe("SuggestionCard alert type", () => {
  const rowCount = () => suggestion({ kind: "row_count", column: "n", target_column: "", guessed_threshold: false });

  it("shows the toggle only for row_count and hides the condition grid in baseline", () => {
    renderCard({ suggestion: rowCount(), monitorKind: "baseline" });

    expect(screen.getByRole("button", { name: "Baseline" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.queryByText("Operator")).not.toBeInTheDocument();
    expect(screen.getByText(/Starts alerting after 7 checks/)).toBeInTheDocument();
  });

  it("calls onMonitorKindChange and restores the grid for threshold", async () => {
    const onMonitorKindChange = vi.fn();
    renderCard({ suggestion: rowCount(), monitorKind: "threshold", onMonitorKindChange });

    expect(screen.getByLabelText("Operator")).toHaveAttribute("role", "combobox");
    expect(screen.getByLabelText("Value")).toHaveAttribute("type", "number");
    await userEvent.click(screen.getByRole("button", { name: "Baseline" }));
    expect(onMonitorKindChange).toHaveBeenCalledWith("baseline");
  });

  it("offers no toggle for freshness", () => {
    renderCard({ monitorKind: "threshold" });

    expect(screen.queryByRole("button", { name: "Baseline" })).not.toBeInTheDocument();
  });

  it("creates a baseline monitor without op/value, even with an empty value", async () => {
    mockedApi.createQuery.mockResolvedValue({ id: 10, working_version: 1 });
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockResolvedValue({ id: 20 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});
    await createSuggestedCheck(1, rowCount(), ">", 0, {}, "baseline");

    expect(mockedApi.createMonitor).toHaveBeenCalledWith(
      expect.objectContaining({ options: { column: "n", kind: "baseline" } }),
    );
  });

  it("baseline test verdict explains learning instead of would-alert", async () => {
    mockedApi.executeAdHoc.mockResolvedValue({ query_result: { data: { rows: [{ n: 120 }], columns: [] } } });
    renderCard({ suggestion: rowCount(), monitorKind: "baseline" });
    await userEvent.click(screen.getByRole("button", { name: "Test" }));

    expect(await screen.findByRole("status")).toHaveTextContent(
      "Row count is 120 now. Baseline checks learn from their first 7 checks before they can alert.",
    );
  });
});
