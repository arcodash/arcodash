/** Required-field markers and the disabled-Add tooltip. */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router";
import { AddWidgetDialog } from "../AddWidgetDialog";

vi.mock("@/lib/api", () => ({
  api: {
    listQueries: vi.fn().mockResolvedValue([{ id: 1, name: "My Query", published: true }]),
    listVisualizations: vi.fn().mockResolvedValue([{ id: 5, name: "My Viz", type: "TABLE" }]),
    listMonitors: vi.fn().mockResolvedValue([
      { id: 1, name: "Order count", state: "ok" },
      { id: 2, name: "Revenue drop", state: "triggered" },
    ]),
    createWidget: vi.fn().mockResolvedValue({ id: 9 }),
  },
  ApiError: class ApiError extends Error {
    status: number;
    constructor(status: number, message: string) {
      super(message);
      this.status = status;
    }
  },
  apiErrorMessage: (err: unknown, fallback: string) =>
    err instanceof Error ? err.message : fallback,
}));

function renderDialog() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <AddWidgetDialog dashboardId={1} open onClose={vi.fn()} />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

describe("AddWidgetDialog", () => {
  beforeEach(() => vi.clearAllMocks());

  it("marks Query/Visualization required and disables Add until both are picked", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    expect(within(dialog).getByText("Query").parentElement).toHaveTextContent("Query *");
    const addButton = within(dialog).getByRole("button", { name: "Add" });
    expect(addButton).toBeDisabled();
    expect(addButton).toHaveAttribute("title", "Select a query");

    const querySelect = within(dialog).getByTestId("add-widget-query-select");
    await user.click(querySelect);
    await user.click(await screen.findByRole("option", { name: "My Query" }));
    expect(querySelect).toHaveTextContent("My Query");

    const vizLabel = within(dialog)
      .getAllByText("Visualization")
      .find((el) => el.tagName === "LABEL");
    expect(vizLabel).toHaveTextContent("Visualization *");
    expect(addButton).toBeDisabled();
    expect(addButton).toHaveAttribute("title", "Select a visualization");

    const vizSelect = await within(dialog).findByTestId("add-widget-viz-select");
    await user.click(vizSelect);
    const option = await screen.findByRole("option", { name: "My Viz · Table" });
    await user.click(option);
    expect(vizSelect).toHaveTextContent("My Viz · Table");

    expect(addButton).toBeEnabled();
    expect(addButton).not.toHaveAttribute("title");
  });

  it("strips the auto-created ✨ prefix and shows the type in title case, joined with ·", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.listVisualizations).mockResolvedValue([
      { id: 5, name: "✨ Customer Orders Detail", type: "TABLE" },
    ] as never);
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    const querySelect = within(dialog).getByTestId("add-widget-query-select");
    await user.click(querySelect);
    await user.click(await screen.findByRole("option", { name: "My Query" }));

    const vizSelect = await within(dialog).findByTestId("add-widget-viz-select");
    await user.click(vizSelect);
    expect(await screen.findByRole("option", { name: "Customer Orders Detail · Table" })).toBeInTheDocument();
    expect(screen.queryByText(/✨/)).not.toBeInTheDocument();
    expect(screen.queryByText(/\(TABLE\)/)).not.toBeInTheDocument();
  });

  it("labels both comboboxes for screen readers", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    expect(within(dialog).getByLabelText("Query", { exact: false })).toBe(
      within(dialog).getByTestId("add-widget-query-select"),
    );

    await user.click(within(dialog).getByTestId("add-widget-query-select"));
    await user.click(await screen.findByRole("option", { name: "My Query" }));

    expect(within(dialog).getByLabelText("Visualization", { exact: false })).toBe(
      await within(dialog).findByTestId("add-widget-viz-select"),
    );
  });

  it("excludes draft queries from the picker, leaving only published ones", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.listQueries).mockResolvedValue([
      { id: 1, name: "My Query", published: true },
      { id: 2, name: "My Draft", published: false },
    ] as never);
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByTestId("add-widget-query-select"));
    expect(await screen.findByRole("option", { name: "My Query" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "My Draft" })).not.toBeInTheDocument();
  });

  it("text mode: Content is required and Add explains why it's disabled", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByRole("button", { name: "Text" }));
    expect(within(dialog).getByText("Content").parentElement).toHaveTextContent("Content *");

    const addButton = within(dialog).getByRole("button", { name: "Add" });
    expect(addButton).toBeDisabled();
    expect(addButton).toHaveAttribute("title", "Enter some text or markdown");

    await user.type(within(dialog).getByPlaceholderText(/enter text or markdown/i), "hello");
    expect(addButton).toBeEnabled();
  });

  it("query with zero visualizations: disables the viz trigger and shows a link to the query editor", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.listVisualizations).mockResolvedValue([]);
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByTestId("add-widget-query-select"));
    await user.click(await screen.findByRole("option", { name: "My Query" }));

    const vizSelect = await within(dialog).findByTestId("add-widget-viz-select");
    expect(vizSelect).toBeDisabled();
    expect(vizSelect).toHaveTextContent("No visualizations yet");

    const link = within(dialog).getByRole("link", { name: /add one in the query editor/i });
    expect(link).toHaveAttribute("href", "/queries/1");
    expect(link).toHaveAttribute("target", "_blank");
    expect(link).toHaveAttribute("rel", "noreferrer");

    const addButton = within(dialog).getByRole("button", { name: "Add" });
    expect(addButton).toBeDisabled();
    expect(addButton).toHaveAttribute("title", "This query has no visualizations yet");
  });

  it("query with visualizations: no empty-state hint, trigger stays enabled", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.listVisualizations).mockResolvedValue([{ id: 5, name: "My Viz", type: "TABLE" }] as never);
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByTestId("add-widget-query-select"));
    await user.click(await screen.findByRole("option", { name: "My Query" }));

    const vizSelect = await within(dialog).findByTestId("add-widget-viz-select");
    expect(vizSelect).toBeEnabled();
    expect(screen.queryByText(/no visualizations yet/i)).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /add one in the query editor/i })).not.toBeInTheDocument();
  });

  it("filters the query options by the search box, and shows an empty state for no matches", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.listQueries).mockResolvedValue([
      { id: 1, name: "Revenue by month", published: true },
      { id: 2, name: "Signups by region", published: true },
    ] as never);
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    const querySelect = within(dialog).getByTestId("add-widget-query-select");
    await user.click(querySelect);
    expect(await screen.findByRole("option", { name: "Revenue by month" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "Signups by region" })).toBeInTheDocument();
    await user.keyboard("{Escape}");

    await user.type(within(dialog).getByPlaceholderText("Search queries…"), "signups");
    await user.click(querySelect);
    expect(await screen.findByRole("option", { name: "Signups by region" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "Revenue by month" })).not.toBeInTheDocument();
    await user.keyboard("{Escape}");

    await user.clear(within(dialog).getByPlaceholderText("Search queries…"));
    await user.type(within(dialog).getByPlaceholderText("Search queries…"), "nothing matches this");
    await user.click(querySelect);
    expect(await screen.findByText("No queries match")).toBeInTheDocument();
    expect(screen.queryByRole("option")).not.toBeInTheDocument();
  });

  it("a create failure shows a destructive error and leaves the dialog open, not just a console.error", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.createWidget).mockRejectedValue(new Error("Dashboard is full"));
    const user = userEvent.setup();
    const onClose = vi.fn();
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <AddWidgetDialog dashboardId={1} open onClose={onClose} />
        </QueryClientProvider>
      </MemoryRouter>,
    );
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByRole("button", { name: "Text" }));
    await user.type(within(dialog).getByPlaceholderText(/enter text or markdown/i), "hello");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("Dashboard is full")).toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("a successful add clears any earlier error", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.createWidget)
      .mockRejectedValueOnce(new Error("first attempt failed"))
      .mockResolvedValueOnce({ id: 9 } as never);
    const user = userEvent.setup();
    const onClose = vi.fn();
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <AddWidgetDialog dashboardId={1} open onClose={onClose} />
        </QueryClientProvider>
      </MemoryRouter>,
    );
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByRole("button", { name: "Text" }));
    await user.type(within(dialog).getByPlaceholderText(/enter text or markdown/i), "hello");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));
    expect(await within(dialog).findByText("first attempt failed")).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it("a failed query list says so instead of 'No queries match'", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.listQueries).mockRejectedValue(new Error("Network down"));
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    const querySelect = within(dialog).getByTestId("add-widget-query-select");
    await user.click(querySelect);
    expect(await screen.findByText("Network down")).toBeInTheDocument();
    expect(screen.queryByText("No queries match")).not.toBeInTheDocument();

    vi.mocked(api.listQueries).mockResolvedValue([{ id: 1, name: "My Query", published: true }] as never);
    await user.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("option", { name: "My Query" })).toBeInTheDocument();
  });

  it("resets the form on a successful add, so reopening doesn't show the previous widget's selections", async () => {
    const { api } = await import("@/lib/api");
    vi.mocked(api.createWidget).mockResolvedValue({ id: 9 } as never);
    const user = userEvent.setup();
    const onClose = vi.fn();
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { rerender } = render(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <AddWidgetDialog dashboardId={1} open onClose={onClose} />
        </QueryClientProvider>
      </MemoryRouter>,
    );
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByRole("button", { name: "Text" }));
    await user.type(within(dialog).getByPlaceholderText(/enter text or markdown/i), "leftover draft");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());

    // Parent closes then reopens: same instance, since AddWidgetDialog stays mounted and is driven
    // by `open`.
    rerender(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <AddWidgetDialog dashboardId={1} open={false} onClose={onClose} />
        </QueryClientProvider>
      </MemoryRouter>,
    );
    rerender(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <AddWidgetDialog dashboardId={1} open onClose={onClose} />
        </QueryClientProvider>
      </MemoryRouter>,
    );

    const reopened = await screen.findByRole("dialog");
    expect(within(reopened).queryByPlaceholderText(/enter text or markdown/i)).not.toBeInTheDocument();
    expect(within(reopened).getByTestId("add-widget-query-select")).toHaveTextContent("Select a query…");
  });

  it("monitor mode: Add is disabled until at least one monitor is selected", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByRole("button", { name: "Monitor" }));
    const addButton = within(dialog).getByRole("button", { name: "Add" });
    expect(addButton).toBeDisabled();
    expect(addButton).toHaveAttribute("title", "Select at least one monitor");

    await screen.findByText("Revenue drop");
    await user.click(within(dialog).getByRole("checkbox", { name: /Revenue drop/i }));
    expect(addButton).toBeEnabled();
  });

  it("monitor mode: selecting two monitors posts both ids, in selection order", async () => {
    const { api } = await import("@/lib/api");
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByRole("button", { name: "Monitor" }));
    await screen.findByText("Revenue drop");
    await user.click(within(dialog).getByRole("checkbox", { name: /Revenue drop/i }));
    await user.click(within(dialog).getByRole("checkbox", { name: /Order count/i }));

    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    await waitFor(() =>
      expect(api.createWidget).toHaveBeenCalledWith(
        expect.objectContaining({
          visualization_id: null,
          text: null,
          options: expect.objectContaining({
            monitors: [2, 1],
            position: { col: 0, row: 0, sizeX: 2, sizeY: 3 },
          }),
        }),
      ),
    );
  });

  it("monitor mode: the search box filters the list by name", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = await screen.findByRole("dialog");

    await user.click(within(dialog).getByRole("button", { name: "Monitor" }));
    await screen.findByText("Revenue drop");
    expect(within(dialog).getByText("Order count")).toBeInTheDocument();

    await user.type(within(dialog).getByTestId("add-widget-monitor-filter"), "revenue");
    expect(within(dialog).queryByText("Order count")).not.toBeInTheDocument();
    expect(within(dialog).getByText("Revenue drop")).toBeInTheDocument();
  });
});
