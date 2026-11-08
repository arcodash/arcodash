import { describe, it, expect, vi, beforeEach } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SuggestMonitorsPane } from "../SuggestMonitorsPane";
import type { QueryMonitorSuggestion, SuggestQueryMonitorsResponse } from "@/types/api";
import type * as ApiModule from "@/lib/api";
import type * as RouterModule from "react-router";

const navigate = vi.fn();
vi.mock("react-router", async (importOriginal) => {
  const actual = await importOriginal<typeof RouterModule>();
  return { ...actual, useNavigate: () => navigate };
});
vi.mock("@/lib/permissions", () => ({ usePermissions: () => ({ has: () => false }) }));
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: { ...actual.api, suggestQueryMonitors: vi.fn(), createMonitor: vi.fn(), createMonitorSubscription: vi.fn() },
  };
});

import { api, ApiError } from "@/lib/api";

const m = api as unknown as {
  suggestQueryMonitors: ReturnType<typeof vi.fn>;
  createMonitor: ReturnType<typeof vi.fn>;
  createMonitorSubscription: ReturnType<typeof vi.fn>;
};

function sug(over: Partial<QueryMonitorSuggestion> = {}): QueryMonitorSuggestion {
  return {
    name: "Refund rate spike",
    kind: "threshold",
    column: "refund_rate",
    sensitivity: null,
    op: ">",
    value: 5,
    definition: "Alerts when `refund_rate` > 5",
    why: "Refunds above `5%` usually mean a broken checkout.",
    confidence: "high",
    ...over,
  };
}

function ok(suggestions: QueryMonitorSuggestion[], extra: Partial<Extract<SuggestQueryMonitorsResponse, { status: "ok" }>> = {}) {
  return { status: "ok", suggestions, already_covered: [], unsupported: null, served_by: "x", ...extra };
}

function renderPane() {
  const onOpenChange = vi.fn();
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <SuggestMonitorsPane queryId={7} queryName="Refunds" hasUnpublishedChanges={false} open onOpenChange={onOpenChange} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

beforeEach(() => {
  vi.clearAllMocks();
  m.createMonitor.mockImplementation(async (b: { name: string }) => ({ id: b.name.length }));
  m.createMonitorSubscription.mockResolvedValue({});
});

describe("SuggestMonitorsPane", () => {
  it("runs suggest-all on open without a prompt and shows 'n of m'", async () => {
    m.suggestQueryMonitors.mockResolvedValue(ok([sug({ name: "A" }), sug({ name: "B" })]));
    renderPane();
    expect(await screen.findByTestId("monitor-suggest-position")).toHaveTextContent("1 of 2");
    expect(m.suggestQueryMonitors).toHaveBeenCalledWith(7, {});
  });

  it("Add, Skip, Add reaches the summary with 2 added; focus follows to the next Add", async () => {
    m.suggestQueryMonitors.mockResolvedValue(ok([sug({ name: "A" }), sug({ name: "BB" }), sug({ name: "CCC" })]));
    const user = userEvent.setup();
    renderPane();
    await user.click(await screen.findByTestId("monitor-suggest-add"));
    await waitFor(() => expect(screen.getByTestId("monitor-suggest-position")).toHaveTextContent("2 of 3"));
    expect(m.createMonitor).toHaveBeenCalledWith({
      query_id: 7,
      name: "A",
      options: { kind: "threshold", column: "refund_rate", op: ">", value: 5 },
    });
    await waitFor(() => expect(screen.getByTestId("monitor-suggest-add")).toHaveFocus());
    await user.click(screen.getByTestId("monitor-suggest-skip"));
    await user.click(await screen.findByTestId("monitor-suggest-add"));
    const summary = await screen.findByTestId("monitor-suggest-summary");
    expect(summary).toHaveTextContent("Added 2 monitors");
    await waitFor(() => expect(screen.getByTestId("monitor-suggest-done")).toHaveFocus());
  });

  it("an empty prompt sends no prompt; a filled one sends it and shows the single card", async () => {
    m.suggestQueryMonitors.mockResolvedValueOnce(ok([])).mockResolvedValueOnce(ok([sug()]));
    const user = userEvent.setup();
    renderPane();
    expect(await screen.findByTestId("monitor-suggest-empty")).toBeInTheDocument();
    await user.type(screen.getByTestId("monitor-suggest-prompt"), "refunds over 5%");
    await user.click(screen.getByTestId("monitor-suggest-submit"));
    expect(await screen.findByTestId("monitor-suggest-position")).toHaveTextContent("From your description");
    expect(m.suggestQueryMonitors).toHaveBeenLastCalledWith(7, { prompt: "refunds over 5%" });
  });

  it("shows the all-covered state", async () => {
    m.suggestQueryMonitors.mockResolvedValue(
      ok([], { already_covered: [{ column: "c", monitor_id: 1, monitor_name: "n", kind: "threshold" }] }),
    );
    renderPane();
    expect(await screen.findByTestId("monitor-suggest-covered")).toBeInTheDocument();
  });

  it("shows the unsupported copy", async () => {
    m.suggestQueryMonitors.mockResolvedValue(ok([], { unsupported: "No refund column." }));
    renderPane();
    const box = await screen.findByTestId("monitor-suggest-unsupported");
    expect(box).toHaveTextContent("Can't build that from this query");
    expect(box).toHaveTextContent("No refund column.");
  });

  it("renders backtick segments as mono text and never as HTML", async () => {
    m.suggestQueryMonitors.mockResolvedValue(
      ok([sug({ definition: "Alerts when `refund_rate` > 5", why: "<b>bold</b> `x`" })]),
    );
    renderPane();
    const card = await screen.findByTestId("monitor-suggest-card");
    expect(card.querySelector("span.font-mono")).toHaveTextContent("refund_rate");
    expect(card.querySelector("b")).toBeNull();
    expect(card).toHaveTextContent("<b>bold</b>");
  });

  it("Edit first navigates with the suggestion as route state", async () => {
    m.suggestQueryMonitors.mockResolvedValue(ok([sug({ confidence: "low" })]));
    const user = userEvent.setup();
    const { onOpenChange } = renderPane();
    await user.click(await screen.findByTestId("monitor-suggest-edit"));
    expect(navigate).toHaveBeenCalledWith("/monitors/new?query_id=7", {
      state: { aiSuggestion: expect.objectContaining({ name: "Refund rate spike", column: "refund_rate", guessed: true }) },
    });
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it("keeps the card and shows an alert when Add fails", async () => {
    m.suggestQueryMonitors.mockResolvedValue(ok([sug()]));
    m.createMonitor.mockRejectedValue(new Error("boom"));
    const user = userEvent.setup();
    renderPane();
    await user.click(await screen.findByTestId("monitor-suggest-add"));
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(screen.getByTestId("monitor-suggest-add")).toHaveTextContent("Add");
  });

  it("shows the server message without Try again for budget and permission errors", async () => {
    for (const err of [new ApiError(429, "Monthly AI budget reached.", "ai_budget_exceeded"), new ApiError(403, "AI is not enabled.")]) {
      m.suggestQueryMonitors.mockRejectedValue(err);
      renderPane();
      expect(await screen.findByText(err.message)).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Try again" })).not.toBeInTheDocument();
      cleanup();
    }
  });

  it("falls back to default copy when Add fails without a message", async () => {
    m.suggestQueryMonitors.mockResolvedValue(ok([sug()]));
    m.createMonitor.mockRejectedValue(new Error(""));
    const user = userEvent.setup();
    renderPane();
    await user.click(await screen.findByTestId("monitor-suggest-add"));
    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't add this monitor.");
  });
});
