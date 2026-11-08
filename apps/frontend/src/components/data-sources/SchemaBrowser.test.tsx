/** Refresh only starts a background job: the button gates on the job state (not the read query's
 * isFetching), the browser polls while `refreshing`, and a failed job is shown inline. */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SchemaBrowser } from "./SchemaBrowser";

vi.mock("@/lib/api", () => ({
  api: {
    getDataSourceSchema: vi.fn(),
  },
  apiErrorMessage: (err: unknown, fallback: string) =>
    err instanceof Error ? err.message : fallback,
}));

function renderBrowser() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SchemaBrowser dataSourceId={1} />
    </QueryClientProvider>,
  );
}

const resp = (
  tableNames: string[],
  over: { refreshing?: boolean; refresh_error?: string | null } = {},
) => ({
  schema: { tables: tableNames.map((name) => ({ name, columns: [{ name: "id", type_name: "int" }] })) },
  refreshing: false,
  refresh_error: null,
  ...over,
});

async function getMock() {
  const { api } = await import("@/lib/api");
  return vi.mocked(api.getDataSourceSchema);
}

describe("SchemaBrowser async refresh", () => {
  beforeEach(() => vi.clearAllMocks());

  it("clicking refresh starts the job, disables the button and shows the refreshing line", async () => {
    const get = await getMock();
    get.mockResolvedValue(resp(["orders"], { refreshing: true }) as never);
    get.mockResolvedValueOnce(resp(["orders"]) as never);
    const user = userEvent.setup();
    renderBrowser();

    const button = await screen.findByTestId("schema-refresh");
    await waitFor(() => expect(button).not.toBeDisabled());
    await user.click(button);

    expect(get).toHaveBeenCalledWith(1, true);
    await waitFor(() => expect(button).toBeDisabled());
    expect(button.querySelector("svg")).toHaveClass("animate-spin");
    expect(button).toHaveAttribute("title", "Refreshing schema…");
    expect(screen.getByTestId("schema-refreshing")).toBeInTheDocument();
    expect(screen.getByText("orders")).toBeInTheDocument();
  });

  it("polls while refreshing and stops once it flips false, rendering the new tables", async () => {
    const get = await getMock();
    get
      .mockResolvedValueOnce(resp(["orders"], { refreshing: true }) as never)
      .mockResolvedValue(resp(["invoices"]) as never);
    renderBrowser();

    expect(await screen.findByText("orders")).toBeInTheDocument();
    expect(await screen.findByText("invoices", {}, { timeout: 5000 })).toBeInTheDocument();
    expect(screen.queryByTestId("schema-refreshing")).not.toBeInTheDocument();
    expect(get).toHaveBeenCalledTimes(2);
    // The interval is 2s; a third call would land in this window if polling had not stopped.
    await new Promise((r) => setTimeout(r, 2500));
    expect(get).toHaveBeenCalledTimes(2);
  }, 15000);

  it("first load with nothing cached shows the loading line instead of 'No tables found.'", async () => {
    const get = await getMock();
    get.mockResolvedValueOnce(resp([], { refreshing: true }) as never).mockResolvedValue(resp(["orders"]) as never);
    renderBrowser();

    expect(await screen.findByTestId("schema-first-load")).toBeInTheDocument();
    expect(screen.queryByText("No tables found.")).not.toBeInTheDocument();
    expect(await screen.findByText("orders", {}, { timeout: 5000 })).toBeInTheDocument();
    expect(screen.queryByTestId("schema-first-load")).not.toBeInTheDocument();
  });

  it("shows 'No tables found.' when empty and not refreshing", async () => {
    const get = await getMock();
    get.mockResolvedValue(resp([]) as never);
    renderBrowser();
    expect(await screen.findByText("No tables found.")).toBeInTheDocument();
  });

  it("shows the refresh error and keeps the old tables listed", async () => {
    const get = await getMock();
    get.mockResolvedValue(resp(["orders"], { refresh_error: "connection refused" }) as never);
    renderBrowser();

    expect(await screen.findByTestId("schema-refresh-error")).toHaveTextContent(
      "Couldn't refresh the schema: connection refused",
    );
    expect(screen.getByText("orders")).toBeInTheDocument();
  });

  it("hides the refresh error while a new refresh is running", async () => {
    const get = await getMock();
    get.mockResolvedValue(resp(["orders"], { refreshing: true, refresh_error: "old failure" }) as never);
    renderBrowser();

    expect(await screen.findByTestId("schema-refreshing")).toBeInTheDocument();
    expect(screen.queryByTestId("schema-refresh-error")).not.toBeInTheDocument();
  });

  it("a failed refresh request surfaces an inline error", async () => {
    const get = await getMock();
    get.mockResolvedValueOnce(resp(["orders"]) as never);
    get.mockRejectedValueOnce(new Error("Warehouse unreachable"));
    const user = userEvent.setup();
    renderBrowser();

    const button = await screen.findByTestId("schema-refresh");
    await waitFor(() => expect(button).not.toBeDisabled());
    await user.click(button);

    expect(await screen.findByText("Warehouse unreachable")).toBeInTheDocument();
    expect(button).not.toBeDisabled();
  });

  it("a repeated click while a refresh is running does not fire another request", async () => {
    const get = await getMock();
    get.mockResolvedValueOnce(resp(["orders"]) as never);
    get.mockResolvedValue(resp(["orders"], { refreshing: true }) as never);
    const user = userEvent.setup();
    renderBrowser();

    const button = await screen.findByTestId("schema-refresh");
    await waitFor(() => expect(button).not.toBeDisabled());
    await user.click(button);
    await user.click(button);
    await user.click(button);

    expect(get.mock.calls.filter(([, refresh]) => refresh === true)).toHaveLength(1);
  });
});
