import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DataSourceEditPage } from "@/pages/DataSourceEditPage";
import type { DataSourceType } from "@/types/api";
import type * as ApiModule from "@/lib/api";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      listDataSourceTypes: vi.fn(),
      getDataSource: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";

const mockedApi = api as unknown as {
  listDataSourceTypes: ReturnType<typeof vi.fn>;
  getDataSource: ReturnType<typeof vi.fn>;
};

const PG_WITH_PASSWORD: DataSourceType = {
  type: "pg",
  name: "PostgreSQL",
  configuration_schema: {
    properties: {
      host: { type: "string", title: "Host" },
      password: { type: "string", title: "Password" },
    },
    required: ["host"],
    secret: ["password"],
  },
};

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/data_sources/9/edit"]}>
        <Routes>
          <Route path="/data_sources/:id/edit" element={<DataSourceEditPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("DataSourceEditPage secret field affordance", () => {
  it("a secret field on the edit form still reads as a stored value with a leave-blank hint, never dots", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue([PG_WITH_PASSWORD]);
    mockedApi.getDataSource.mockResolvedValue({
      id: 9,
      name: "Prod PG",
      type: "pg",
      options: { host: "db.internal" },
    });

    renderPage();

    const passwordInput = await screen.findByLabelText("Password");
    expect(passwordInput).toHaveAttribute("placeholder", "Leave blank to keep current");
    expect(passwordInput).toHaveValue("");
    expect(screen.getByText("Leave empty to keep the current value.")).toBeInTheDocument();
  });
});
