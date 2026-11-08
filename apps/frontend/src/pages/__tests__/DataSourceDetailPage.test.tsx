import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DataSourceDetailPage } from "../DataSourceDetailPage";
import { ApiError } from "@/lib/api";
import type * as ApiModule from "@/lib/api";
import type { DataSource } from "@/types/api";

vi.mock("@/lib/permissions", () => ({
  usePermissions: () => ({ has: () => false, isLoading: false, hasAny: () => false, isAdmin: false, keys: new Set() }),
}));
// Both cards issue their own queries unrelated to the delete flow under test; stub them out
// so the test only depends on data-source endpoints.
vi.mock("@/components/data-sources/SchemaBrowser", () => ({
  SchemaBrowser: () => null,
}));
vi.mock("@/components/data-sources/DataQualityChecksCard", () => ({
  DataQualityChecksCard: () => null,
}));
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      getDataSource: vi.fn(),
      deleteDataSource: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";

const mockedApi = api as unknown as {
  getDataSource: ReturnType<typeof vi.fn>;
  deleteDataSource: ReturnType<typeof vi.fn>;
};

function dataSource(overrides: Partial<DataSource> = {}): DataSource {
  return {
    id: 1,
    name: "My Postgres",
    type: "pg",
    type_display_name: "PostgreSQL",
    schema: null,
    schema_refreshed_at: null,
    paused: false,
    pause_reason: null,
    health_state: "unknown",
    health_error: null,
    health_checked_at: null,
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    caller: { object_role: "owner", can_edit: true, can_delete: true },
    ...overrides,
  };
}

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/data_sources/1"]}>
        <Routes>
          <Route path="/data_sources/:id" element={<DataSourceDetailPage />} />
          <Route path="/data_sources" element={<p>Data sources list</p>} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("DataSourceDetailPage delete flow", () => {
  it("describes the real refusal behavior instead of claiming queries will break", async () => {
    mockedApi.getDataSource.mockResolvedValue(dataSource());
    const user = userEvent.setup();
    renderPage();

    await user.click(await screen.findByRole("button", { name: /delete/i }));
    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).getByText(/data sources still used by a query can't be deleted/i),
    ).toBeInTheDocument();
    expect(
      within(dialog).queryByText(/any queries using it will stop working/i),
    ).not.toBeInTheDocument();
  });

  it("surfaces the backend's conflict message instead of leaving the dialog silently stuck", async () => {
    mockedApi.getDataSource.mockResolvedValue(dataSource());
    mockedApi.deleteDataSource.mockRejectedValue(
      new ApiError(409, "3 queries still use this data source"),
    );
    const user = userEvent.setup();
    renderPage();

    await user.click(await screen.findByRole("button", { name: /delete/i }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Delete" }));

    await waitFor(() =>
      expect(within(dialog).getByText("3 queries still use this data source")).toBeInTheDocument(),
    );
    // The confirm dialog stays open/available rather than silently vanishing on failure,
    // and the error renders inside it, not behind its overlay.
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });
});
