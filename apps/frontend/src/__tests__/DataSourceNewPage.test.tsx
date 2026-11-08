import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DataSourceNewPage } from "@/pages/DataSourceNewPage";
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

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <DataSourceNewPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return qc;
}

describe("DataSourceNewPage type default", () => {
  it("starts with no type selected and the connection fields hidden", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue([
      PG_TYPE,
      { type: "athena", name: "Amazon Athena", configuration_schema: { properties: { access_key: { type: "string", title: "Access key" } }, required: ["access_key"] } },
    ]);

    renderPage();

    await screen.findByLabelText(/^Name/);
    expect(screen.getByText("Select a data source type")).toBeInTheDocument();
    expect(screen.queryByLabelText(/Host/)).not.toBeInTheDocument();
    expect(screen.queryByLabelText(/Access key/)).not.toBeInTheDocument();
  });

  it("Name placeholder is type-agnostic until a type is picked, then follows the selection", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue([PG_TYPE]);
    const user = userEvent.setup();

    renderPage();

    const nameInput = await screen.findByLabelText(/^Name/);
    expect(nameInput).toHaveAttribute("placeholder", "My database");

    await user.click(screen.getByRole("combobox", { name: /^Type/ }));
    await user.click(await screen.findByRole("option", { name: "PostgreSQL" }));

    expect(nameInput).toHaveAttribute("placeholder", "My PostgreSQL database");
  });
});

describe("DataSourceNewPage secret field placeholder", () => {
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

  it("an empty secret field on the create form carries no placeholder — dots would read as a stored value that doesn't exist", async () => {
    mockedApi.listDataSourceTypes.mockResolvedValue([PG_WITH_PASSWORD]);
    const user = userEvent.setup();

    renderPage();

    await screen.findByLabelText(/^Name/);
    await user.click(screen.getByRole("combobox", { name: /^Type/ }));
    await user.click(await screen.findByRole("option", { name: "PostgreSQL" }));

    const passwordInput = await screen.findByLabelText("Password");
    expect(passwordInput).not.toHaveAttribute("placeholder", "••••••••");
    expect(passwordInput).not.toHaveAttribute("placeholder");
  });
});
