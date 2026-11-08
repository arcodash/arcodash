import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DataSourceNewPage } from "../DataSourceNewPage";
import type * as ApiModule from "@/lib/api";
import type { DataSourceType } from "@/types/api";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      listDataSourceTypes: vi.fn(),
      createDataSource: vi.fn(),
      testDataSource: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";

const mockedApi = api as unknown as {
  listDataSourceTypes: ReturnType<typeof vi.fn>;
  createDataSource: ReturnType<typeof vi.fn>;
  testDataSource: ReturnType<typeof vi.fn>;
};

const TYPES: DataSourceType[] = [
  {
    type: "pg",
    name: "PostgreSQL",
    configuration_schema: {
      properties: {
        host: { type: "string", title: "Host" },
      },
      required: ["host"],
      secret: [],
    },
  },
];

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <DataSourceNewPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("DataSourceNewPage", () => {
  // A disabled submit button hides *why* it's disabled — the form validates on submit instead
  // and shows an in-app error, so Create stays clickable even with no type/host filled in yet.
  it("validates on submit instead of disabling Create, and shows an in-app error (not the native bubble)", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue(TYPES);
    const user = userEvent.setup();
    renderPage();

    const createButton = await screen.findByRole("button", { name: "Create" });
    expect(createButton).not.toBeDisabled();

    await user.click(createButton);
    expect(await screen.findByText("Type is required.")).toBeInTheDocument();
    expect(mockedApi.createDataSource).not.toHaveBeenCalled();

    await user.click(screen.getByRole("combobox", { name: /^Type/ }));
    await user.click(await screen.findByRole("option", { name: "PostgreSQL" }));
    await user.type(await screen.findByLabelText(/Name/), "Prod PG");
    await user.type(screen.getByLabelText(/Host/), "db.internal");
    await user.click(createButton);

    await waitFor(() => expect(mockedApi.createDataSource).toHaveBeenCalled());
  });
});
