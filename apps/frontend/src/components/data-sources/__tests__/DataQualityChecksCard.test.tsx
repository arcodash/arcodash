import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DataQualityChecksCard } from "../DataQualityChecksCard";
import type { ClientConfig } from "@/types/api";
import type * as ApiModule from "@/lib/api";

vi.mock("@/lib/auth", () => ({
  useAuth: vi.fn(),
}));
vi.mock("@/lib/permissions", () => ({
  usePermissions: vi.fn(),
}));
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      getDataSourceSchema: vi.fn(),
      suggestMonitors: vi.fn(),
      executeAdHoc: vi.fn(),
      createQuery: vi.fn(),
      publishQuery: vi.fn(),
      createMonitor: vi.fn(),
      createMonitorSubscription: vi.fn(),
    },
  };
});

import { useAuth } from "@/lib/auth";
import { usePermissions } from "@/lib/permissions";
import { api } from "@/lib/api";

const mockedApi = api as unknown as {
  getDataSourceSchema: ReturnType<typeof vi.fn>;
  suggestMonitors: ReturnType<typeof vi.fn>;
  executeAdHoc: ReturnType<typeof vi.fn>;
  createQuery: ReturnType<typeof vi.fn>;
  publishQuery: ReturnType<typeof vi.fn>;
  createMonitor: ReturnType<typeof vi.fn>;
  createMonitorSubscription: ReturnType<typeof vi.fn>;
};

function setClientConfig(overrides: Partial<ClientConfig> = {}) {
  const clientConfig: ClientConfig = {
    disable_public_urls: false,
    saml_enabled: false,
    ldap_enabled: false,
    ai_sql_generation: true,
    ai_monitor_suggestions: true,
    ...overrides,
  };
  (useAuth as unknown as ReturnType<typeof vi.fn>).mockReturnValue({ clientConfig });
}

function setPermissions(opts: { has?: (key: string) => boolean; isAdmin?: boolean } = {}) {
  (usePermissions as unknown as ReturnType<typeof vi.fn>).mockReturnValue({
    has: opts.has ?? (() => true),
    isAdmin: opts.isAdmin ?? true,
    hasAny: () => true,
    isLoading: false,
    keys: new Set<string>(),
  });
}

function tableNames(names: string[]) {
  return names.map((name) => ({ name, columns: [{ name: "id", type_name: "bigint" }] }));
}

function renderCard(dataSourceId = 1) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const result = render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <DataQualityChecksCard dataSourceId={dataSourceId} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return { ...result, queryClient: qc };
}

function suggestion(overrides: Record<string, unknown> = {}) {
  return {
    table: "orders",
    kind: "null_rate",
    title: "Orders null rate",
    rationale: "Alerts on unexpected nulls.",
    sql: "SELECT 100.0 AS null_pct",
    column: "null_pct",
    target_column: "email",
    op: ">",
    value: 5,
    guessed_threshold: true,
    ...overrides,
  };
}

describe("DataQualityChecksCard gate matrix", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mockedApi.getDataSourceSchema.mockResolvedValue({ schema: { tables: [] } });
  });

  it("renders null when ai_monitor_suggestions is false", () => {
    setClientConfig({ ai_monitor_suggestions: false });
    setPermissions();
    const { container } = renderCard();
    expect(container).toBeEmptyDOMElement();
  });

  it("renders null when the caller lacks create_query", () => {
    setClientConfig();
    setPermissions({ has: (k) => k !== "create_query" });
    const { container } = renderCard();
    expect(container).toBeEmptyDOMElement();
  });

  it("renders null when the caller lacks create_monitor", () => {
    setClientConfig();
    setPermissions({ has: (k) => k !== "create_monitor" });
    const { container } = renderCard();
    expect(container).toBeEmptyDOMElement();
  });

  it("shows the admin 'Add an AI provider' line when no provider is configured", async () => {
    setClientConfig({ ai_sql_generation: false });
    setPermissions({ isAdmin: true });
    renderCard();
    expect(await screen.findByText(/Add an AI provider/)).toBeInTheDocument();
  });

  it("shows the 'ask an admin' line for a non-admin when no provider is configured", async () => {
    setClientConfig({ ai_sql_generation: false });
    setPermissions({ isAdmin: false });
    renderCard();
    expect(await screen.findByText(/Ask an admin to add an AI provider/)).toBeInTheDocument();
  });
});

describe("DataQualityChecksCard results rendering", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    setClientConfig();
    setPermissions();
  });

  it("renders both cards when two same-kind suggestions land on the same table", async () => {
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders"]) },
    });
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "ok",
      suggestions: [
        suggestion({ kind: "null_rate", title: "Nulls in email", column: "null_pct" }),
        suggestion({ kind: "null_rate", title: "Nulls in phone", column: "null_pct" }),
      ],
      covered: [],
      scanned_tables: 1,
      served_by: "openai",
    });

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));

    expect(await screen.findByText("Nulls in email")).toBeInTheDocument();
    expect(await screen.findByText("Nulls in phone")).toBeInTheDocument();
    expect(screen.getByText(/2 checks suggested/)).toBeInTheDocument();
  });

  it("runs immediately over all tables when at or under 20", async () => {
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders", "customers"]) },
    });
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "ok",
      suggestions: [],
      covered: [],
      scanned_tables: 2,
      served_by: "openai",
    });

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));

    await waitFor(() =>
      expect(mockedApi.suggestMonitors).toHaveBeenCalledWith(
        1,
        { tables: ["orders", "customers"] },
        expect.anything(),
      ),
    );
  });

  it("opens the scope picker above 20 tables and disables the button at 0 and >20 selected", async () => {
    const names = Array.from({ length: 21 }, (_, i) => `t${i}`);
    mockedApi.getDataSourceSchema.mockResolvedValue({ schema: { tables: tableNames(names) } });

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));

    expect(await screen.findByText(/This data source has 21 tables/)).toBeInTheDocument();
    const runButton = screen.getByRole("button", { name: /Suggest checks for 0 tables/ });
    expect(runButton).toBeDisabled();

    for (const name of names) {
      await userEvent.click(screen.getByRole("checkbox", { name: new RegExp(`^${name}$`) }));
    }
    expect(screen.getByRole("button", { name: /Suggest checks for 20 tables/ })).not.toBeDisabled();
  });

  it("Cancel aborts the run and leaves no error banner", async () => {
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders"]) },
    });
    mockedApi.suggestMonitors.mockImplementation(
      (_dsId: number, _body: unknown, signal?: AbortSignal) =>
        new Promise((_resolve, reject) => {
          signal?.addEventListener("abort", () =>
            reject(new DOMException("The operation was aborted.", "AbortError")),
          );
        }),
    );

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));

    await screen.findByText("Looking at your schema…");
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await waitFor(() =>
      expect(screen.queryByText("Looking at your schema…")).not.toBeInTheDocument(),
    );
    expect(screen.queryByText(/Failed to suggest checks/)).not.toBeInTheDocument();
  });

  it("renders a covered table as a muted row linking to the existing monitor", async () => {
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders"]) },
    });
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "ok",
      suggestions: [],
      covered: [{ table: "orders", monitor_id: 42, monitor_name: "Existing" }],
      scanned_tables: 1,
      served_by: "openai",
    });

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));

    const link = await screen.findByRole("link", { name: "View monitor" });
    expect(link).toHaveAttribute("href", "/monitors/42");
    expect(screen.getByText(/Already covered by an existing check/)).toBeInTheDocument();
  });

  it("dismiss-all shows 'Nothing left here.' with a Try again button", async () => {
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders"]) },
    });
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "ok",
      suggestions: [suggestion()],
      covered: [],
      scanned_tables: 1,
      served_by: "openai",
    });

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    await screen.findByText("Orders null rate");

    await userEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(await screen.findByText("Nothing left here.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("renders the rate_limited error banner with the retry-after suffix", async () => {
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders"]) },
    });
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "rate_limited",
      message: "Too many requests.",
      retry_after_secs: 34,
    });

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));

    expect(await screen.findByText(/Too many requests\. \(try again in 34s\)/)).toBeInTheDocument();
  });

  it("keeps the primary CTA visible and clickable after an error, at or under 20 tables", async () => {
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders"]) },
    });
    mockedApi.suggestMonitors.mockRejectedValueOnce(new Error("boom"));

    renderCard();
    const primaryButton = await screen.findByRole("button", { name: "Suggest checks with AI" });
    await userEvent.click(primaryButton);

    await screen.findByText(/Failed to suggest checks/);
    const sameButton = screen.getByRole("button", { name: "Suggest checks with AI" });
    expect(sameButton).toBeInTheDocument();
    expect(sameButton).toBeEnabled();

    mockedApi.suggestMonitors.mockResolvedValueOnce({
      status: "ok",
      suggestions: [],
      covered: [],
      scanned_tables: 1,
      served_by: "openai",
    });
    await userEvent.click(sameButton);
    await waitFor(() => expect(screen.queryByText(/Failed to suggest checks/)).not.toBeInTheDocument());
  });

  it("retry drops a table removed from the schema since the failed run", async () => {
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders", "customers"]) },
    });
    mockedApi.suggestMonitors.mockRejectedValueOnce(new Error("boom"));

    const { queryClient } = renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    await screen.findByText(/Failed to suggest checks/);
    expect(mockedApi.suggestMonitors).toHaveBeenCalledWith(
      1,
      { tables: ["orders", "customers"] },
      expect.anything(),
    );

    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders"]) },
    });
    await queryClient.refetchQueries({ queryKey: ["data-source-schema", 1] });
    mockedApi.suggestMonitors.mockResolvedValueOnce({
      status: "ok",
      suggestions: [],
      covered: [],
      scanned_tables: 1,
      served_by: "openai",
    });

    await userEvent.click(screen.getByRole("button", { name: "Try again" }));

    await waitFor(() =>
      expect(mockedApi.suggestMonitors).toHaveBeenLastCalledWith(1, { tables: ["orders"] }, expect.anything()),
    );
  });

  it("shows the real schema-fetch error and never calls suggestMonitors when the schema failed to load", async () => {
    mockedApi.getDataSourceSchema.mockRejectedValue(new Error("boom"));

    renderCard();
    const primaryButton = await screen.findByRole("button", { name: "Suggest checks with AI" });
    await userEvent.click(primaryButton);

    expect(await screen.findByText("Failed to load schema")).toBeInTheDocument();
    expect(screen.queryByText(/tables must not be empty/)).not.toBeInTheDocument();
    expect(mockedApi.suggestMonitors).not.toHaveBeenCalled();
  });

  it("Try again after a schema-load failure actually refetches the schema, not just re-reads the stale error", async () => {
    mockedApi.getDataSourceSchema.mockRejectedValueOnce(new Error("boom"));

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    expect(await screen.findByText("Failed to load schema")).toBeInTheDocument();
    expect(mockedApi.getDataSourceSchema).toHaveBeenCalledTimes(1);

    mockedApi.getDataSourceSchema.mockResolvedValueOnce({
      schema: { tables: tableNames(["orders"]) },
    });
    mockedApi.suggestMonitors.mockResolvedValueOnce({
      status: "ok",
      suggestions: [],
      covered: [],
      scanned_tables: 1,
      served_by: "openai",
    });

    await userEvent.click(screen.getByRole("button", { name: "Try again" }));

    await waitFor(() => expect(mockedApi.getDataSourceSchema).toHaveBeenCalledTimes(2));
    expect(screen.queryByText("Failed to load schema")).not.toBeInTheDocument();
    await waitFor(() =>
      expect(mockedApi.suggestMonitors).toHaveBeenCalledWith(1, { tables: ["orders"] }, expect.anything()),
    );
  });

  it("Try again after a schema-load failure shows the fresh error when the refetch fails too", async () => {
    mockedApi.getDataSourceSchema.mockRejectedValueOnce(new Error("first failure"));

    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    expect(await screen.findByText("Failed to load schema")).toBeInTheDocument();

    mockedApi.getDataSourceSchema.mockRejectedValueOnce(new Error("second failure"));
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));

    await waitFor(() => expect(mockedApi.getDataSourceSchema).toHaveBeenCalledTimes(2));
    expect(await screen.findByText("Failed to load schema")).toBeInTheDocument();
    expect(mockedApi.suggestMonitors).not.toHaveBeenCalled();
  });

  it("retry never exceeds MAX_TABLES_PER_RUN when lastRunTables is null or fully stale", async () => {
    const names = Array.from({ length: 25 }, (_, i) => `t${i}`);
    mockedApi.getDataSourceSchema.mockResolvedValue({ schema: { tables: tableNames(names) } });

    const { queryClient } = renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    await screen.findByText(/This data source has 25 tables/);

    // After the schema refresh drops every selected table, the retry must fall back to the
    // first-run gate, not resend a vanished list.
    mockedApi.suggestMonitors.mockRejectedValueOnce(new Error("boom"));
    for (const name of names.slice(0, 3)) {
      await userEvent.click(screen.getByRole("checkbox", { name: new RegExp(`^${name}$`) }));
    }
    await userEvent.click(screen.getByRole("button", { name: /Suggest checks for 3 tables/ }));
    await screen.findByText(/Failed to suggest checks/);

    const remaining = names.slice(3);
    mockedApi.getDataSourceSchema.mockResolvedValue({ schema: { tables: tableNames(remaining) } });
    await queryClient.refetchQueries({ queryKey: ["data-source-schema", 1] });

    await userEvent.click(screen.getByRole("button", { name: "Try again" }));

    // 22 tables still exceeds MAX_TABLES_PER_RUN, so this must reopen the scope picker.
    await screen.findByText(/This data source has 22 tables/);
    expect(mockedApi.suggestMonitors).toHaveBeenCalledTimes(1);
  });
});

describe("DataQualityChecksCard batch create", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    setClientConfig();
    setPermissions();
    mockedApi.getDataSourceSchema.mockResolvedValue({
      schema: { tables: tableNames(["orders", "customers"]) },
    });
  });

  async function runAndSelectTwo() {
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "ok",
      suggestions: [
        suggestion({ table: "orders", title: "Check A" }),
        suggestion({ table: "customers", title: "Check B" }),
      ],
      covered: [],
      scanned_tables: 2,
      served_by: "openai",
    });
    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    await screen.findByText("Check A");
    const checkboxes = screen.getAllByRole("checkbox", { name: /^Select/ });
    for (const cb of checkboxes) await userEvent.click(cb);
  }

  it("creates sequentially and reports a partial failure count", async () => {
    await runAndSelectTwo();

    mockedApi.createQuery
      .mockResolvedValueOnce({ id: 1, working_version: 0 })
      .mockRejectedValueOnce(new Error("boom"));
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockResolvedValue({ id: 100 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    await userEvent.click(screen.getByRole("button", { name: /Create 2 checks/ }));

    expect(await screen.findByText(/1 of 2 checks created/)).toBeInTheDocument();
    expect(mockedApi.createQuery).toHaveBeenCalledTimes(2);
  });

  it("a retry after a batch failure reuses the query already created instead of minting a second one", async () => {
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "ok",
      suggestions: [suggestion({ table: "orders", title: "Check A" })],
      covered: [],
      scanned_tables: 1,
      served_by: "openai",
    });
    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    await screen.findByText("Check A");
    await userEvent.click(screen.getByRole("checkbox", { name: /^Select/ }));

    mockedApi.createQuery.mockResolvedValue({ id: 1, working_version: 0 });
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockRejectedValueOnce(new Error("boom")).mockResolvedValueOnce({ id: 100 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    await userEvent.click(screen.getByRole("button", { name: /Create 1 checks/ }));
    await screen.findByText("Failed to create check");

    await userEvent.click(screen.getByRole("button", { name: "Create check" }));

    await waitFor(() => expect(mockedApi.createMonitor).toHaveBeenCalledTimes(2));
    expect(mockedApi.createQuery).toHaveBeenCalledTimes(1);
    expect(mockedApi.publishQuery).toHaveBeenCalledTimes(1);
  });

  it("a suggestion whose test failed is dropped from the batch even if still marked selected", async () => {
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "ok",
      suggestions: [
        suggestion({ table: "orders", title: "Check A" }),
        suggestion({ table: "customers", title: "Check B" }),
      ],
      covered: [],
      scanned_tables: 2,
      served_by: "openai",
    });
    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    await screen.findByText("Check A");
    const checkboxes = screen.getAllByRole("checkbox", { name: /^Select/ });
    for (const cb of checkboxes) await userEvent.click(cb);

    // A check that failed after selection must be excluded by the batch filter's guard.
    mockedApi.executeAdHoc.mockRejectedValueOnce(new Error("bad sql"));
    const testButtons = screen.getAllByRole("button", { name: "Test" });
    await userEvent.click(testButtons[0]);
    await screen.findByText(/Can't evaluate/);

    mockedApi.createQuery.mockResolvedValue({ id: 1, working_version: 0 });
    mockedApi.publishQuery.mockResolvedValue({});
    mockedApi.createMonitor.mockResolvedValue({ id: 100 });
    mockedApi.createMonitorSubscription.mockResolvedValue({});

    await userEvent.click(screen.getByRole("button", { name: /Create 1 checks/ }));

    await waitFor(() => expect(mockedApi.createQuery).toHaveBeenCalledTimes(1));
    expect(mockedApi.createQuery).toHaveBeenCalledWith(expect.objectContaining({ name: "Check B" }));
  });

  it("footer only appears after a selection is made", async () => {
    mockedApi.suggestMonitors.mockResolvedValue({
      status: "ok",
      suggestions: [suggestion({ table: "orders", title: "Check A" })],
      covered: [],
      scanned_tables: 1,
      served_by: "openai",
    });
    renderCard();
    await userEvent.click(await screen.findByRole("button", { name: "Suggest checks with AI" }));
    await screen.findByText("Check A");

    expect(screen.queryByRole("button", { name: /Create \d+ checks/ })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("checkbox", { name: /^Select/ }));
    expect(screen.getByRole("button", { name: /Create 1 checks/ })).toBeInTheDocument();
  });
});
