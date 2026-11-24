import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, useLocation } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MonitorsPage } from "../MonitorsPage";
import type * as ApiModule from "@/lib/api";

vi.mock("@/lib/permissions", () => ({
  usePermissions: () => ({ has: () => true, isLoading: false, hasAny: () => true, isAdmin: true, keys: new Set() }),
}));
vi.mock("@/lib/auth", () => ({ useAuth: () => ({ clientConfig: {} }) }));
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  const api = Object.fromEntries(
    Object.keys(actual.api).map((k) => [k, vi.fn(async () => [])]),
  ) as unknown as typeof actual.api;
  return { ...actual, api };
});

import { api } from "@/lib/api";

const mocked = api as unknown as Record<string, ReturnType<typeof vi.fn>>;

function Search() {
  return <div data-testid="search">{useLocation().search}</div>;
}

function renderAt(url: string) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter initialEntries={[url]}>
      <QueryClientProvider client={qc}>
        <MonitorsPage />
        <Search />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

describe("MonitorsPage query filter", () => {
  beforeEach(() => mocked.listMonitors.mockClear());

  it("filters by the query_id param and clears it", async () => {
    renderAt("/monitors?query_id=42");

    await waitFor(() => expect(mocked.listMonitors).toHaveBeenCalledWith(undefined, 42));
    await userEvent.click(screen.getByText("Filtered to one query · clear"));

    expect(screen.getByTestId("search")).toHaveTextContent(/^$/);
    expect(screen.queryByText("Filtered to one query · clear")).not.toBeInTheDocument();
    await waitFor(() => expect(mocked.listMonitors).toHaveBeenLastCalledWith(undefined, undefined));
  });

  it("shows no filter note without the param", async () => {
    renderAt("/monitors");

    await waitFor(() => expect(mocked.listMonitors).toHaveBeenCalled());
    expect(screen.queryByText("Filtered to one query · clear")).not.toBeInTheDocument();
  });
});
