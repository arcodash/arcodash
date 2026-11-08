import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { CreateDataSourceWizard } from "../CreateDataSourceWizard";
import type { DataSourceType } from "@/types/api";
import type * as ApiModule from "@/lib/api";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      listDataSourceTypes: vi.fn(),
      createDataSource: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";

const mockedApi = api as unknown as {
  listDataSourceTypes: ReturnType<typeof vi.fn>;
  createDataSource: ReturnType<typeof vi.fn>;
};

const PG_TYPE: DataSourceType = {
  type: "pg",
  name: "PostgreSQL",
  configuration_schema: {
    properties: { host: { type: "string", title: "Host" } },
    required: ["host"],
  },
};

const INFLUXDB_TYPE: DataSourceType = {
  type: "influxdb",
  name: "InfluxDB",
  configuration_schema: { properties: {}, required: [] },
};

const MYSQL_TYPE: DataSourceType = {
  type: "mysql",
  name: "MySQL",
  configuration_schema: { properties: {}, required: [] },
};

function renderWizard() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <CreateDataSourceWizard open onOpenChange={() => {}} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return qc;
}

describe("CreateDataSourceWizard create", () => {
  it("creates from the tile picker and invalidates the data source cache", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue([PG_TYPE]);
    mockedApi.createDataSource.mockResolvedValue({ id: 5, name: "Prod PG", type: "pg" });
    const user = userEvent.setup();

    const qc = renderWizard();
    const invalidateSpy = vi.spyOn(qc, "invalidateQueries");

    await user.click(await screen.findByRole("button", { name: /PostgreSQL/ }));
    await user.type(await screen.findByLabelText(/^Name/), "Prod PG");
    await user.type(screen.getByLabelText(/Host/), "db.internal");
    await user.click(screen.getByRole("button", { name: "Create" }));

    await vi.waitFor(() => expect(invalidateSpy).toHaveBeenCalled());
    expect(mockedApi.createDataSource).toHaveBeenCalledTimes(1);
  });
});

describe("CreateDataSourceWizard picker — search and order", () => {
  it("orders Databases by popularity first, not the order the API returned them in", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue([INFLUXDB_TYPE, PG_TYPE, MYSQL_TYPE]);
    renderWizard();

    const tiles = await screen.findAllByRole("button", { name: /PostgreSQL|MySQL|InfluxDB/ });
    expect(tiles.map((t) => t.textContent)).toEqual(["PostgreSQL", "MySQL", "InfluxDB"]);
  });

  it("filters tiles by a case-insensitive substring of the display name or the raw type", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue([INFLUXDB_TYPE, PG_TYPE, MYSQL_TYPE]);
    const user = userEvent.setup();
    renderWizard();

    await screen.findByRole("button", { name: /PostgreSQL/ });
    await user.type(screen.getByRole("textbox", { name: /search data source types/i }), "pg");

    expect(screen.getByRole("button", { name: /PostgreSQL/ })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /MySQL/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /InfluxDB/ })).not.toBeInTheDocument();
  });

  it("shows a clearable no-matches message when nothing matches", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue([PG_TYPE]);
    const user = userEvent.setup();
    renderWizard();

    await screen.findByRole("button", { name: /PostgreSQL/ });
    await user.type(screen.getByRole("textbox", { name: /search data source types/i }), "nonexistent");

    expect(await screen.findByText(/No data source type matches/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Clear search" }));
    expect(await screen.findByRole("button", { name: /PostgreSQL/ })).toBeInTheDocument();
  });
});
