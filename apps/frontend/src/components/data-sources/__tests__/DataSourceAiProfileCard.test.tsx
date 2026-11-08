import { describe, it, expect, vi, beforeEach } from "vitest";
import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DataSourceAiProfileCard } from "../DataSourceAiProfileCard";
import type { DataSourceAiProfileResponse } from "@/types/api";
import type * as ApiModule from "@/lib/api";

vi.mock("@/lib/permissions", () => ({ usePermissions: vi.fn() }));
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      getDataSourceAiProfile: vi.fn(),
      rebuildDataSourceAiProfile: vi.fn(),
      getAiSettings: vi.fn(),
    },
  };
});

import { usePermissions } from "@/lib/permissions";
import { api } from "@/lib/api";

const mocked = api as unknown as {
  getDataSourceAiProfile: ReturnType<typeof vi.fn>;
  rebuildDataSourceAiProfile: ReturnType<typeof vi.fn>;
  getAiSettings: ReturnType<typeof vi.fn>;
};

function setAdmin(isAdmin: boolean) {
  vi.mocked(usePermissions).mockReturnValue({
    keys: new Set(),
    isAdmin,
    isLoading: false,
    has: () => isAdmin,
    hasAny: () => isAdmin,
  });
}

const base: DataSourceAiProfileResponse = {
  profile: null,
  built_at: null,
  supported: true,
  building: false,
  current: false,
};

const profile = {
  tables: [
    {
      name: "orders",
      sampled_rows: 200,
      columns: [
        { name: "id", null_frac: 0, distinct_in_sample: 200, samples: ["1", "2"] },
        { name: "status", null_frac: 0.256, distinct_in_sample: 3, samples: ["paid"], description: "State" },
      ],
    },
    { name: "empty", sampled_rows: 0, columns: [] },
  ],
  relationships: [
    { from_table: "orders", from_column: "user_id", to_table: "users", to_column: "id", source: "fk" as const },
    { from_table: "a", from_column: "x", to_table: "b", to_column: "y", source: "mined" as const },
    { from_table: "c", from_column: "x", to_table: "d", to_column: "y", source: "name" as const },
  ],
  schema_refreshed_at_seen: "2026-10-04T00:00:00Z",
  descriptions_used_samples: true,
};

const built = {
  ...base,
  profile,
  built_at: "2026-10-04T00:00:00Z",
  current: true,
};

function renderCard(
  schemaRefreshedAt: string | null = "2026-10-04T00:00:00Z",
  qc = new QueryClient({ defaultOptions: { queries: { retry: false } } }),
  initialEntry = "/",
) {
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[initialEntry]}>
        <DataSourceAiProfileCard dataSourceId={7} typeDisplayName="PostgreSQL" schemaRefreshedAt={schemaRefreshedAt} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  setAdmin(true);
  mocked.getAiSettings.mockResolvedValue({ include_sample_values: true });
});

describe("DataSourceAiProfileCard", () => {
  it("picks up a schema loaded after mount: hint gives way to Build profile", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue(base);
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderCard(null, qc);
    expect(await screen.findByText(/Load the schema below first/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Build profile" })).not.toBeInTheDocument();
    act(() => {
      qc.setQueryData(["data-source-schema", 7], {
        schema: { tables: [{ name: "orders", columns: [{ name: "id" }] }] },
      });
    });
    expect(await screen.findByRole("button", { name: "Build profile" })).toBeInTheDocument();
    expect(screen.getByText("Not built yet")).toBeInTheDocument();
    expect(screen.queryByText(/Load the schema below first/)).not.toBeInTheDocument();
  });

  it("scrolls to the card once when the URL hash targets it", async () => {
    const scroll = vi.fn();
    Element.prototype.scrollIntoView = scroll;
    mocked.getDataSourceAiProfile.mockResolvedValue(built);
    renderCard(undefined, undefined, "/#ai-profile");
    await screen.findByText(/Ready · built/);
    expect(scroll).toHaveBeenCalledTimes(1);
  });

  it("does not scroll without the hash", async () => {
    const scroll = vi.fn();
    Element.prototype.scrollIntoView = scroll;
    mocked.getDataSourceAiProfile.mockResolvedValue(built);
    renderCard();
    await screen.findByText(/Ready · built/);
    expect(scroll).not.toHaveBeenCalled();
  });

  it("renders nothing and makes no request for a non-admin", () => {
    setAdmin(false);
    const { container } = renderCard();
    expect(container).toBeEmptyDOMElement();
    expect(mocked.getDataSourceAiProfile).not.toHaveBeenCalled();
  });

  it("never built: Build profile calls rebuild and flips to a disabled Updating…", async () => {
    mocked.getDataSourceAiProfile
      .mockResolvedValueOnce(base)
      .mockResolvedValue({ ...base, building: true });
    mocked.rebuildDataSourceAiProfile.mockResolvedValue({ status: "queued" });
    const user = userEvent.setup();
    renderCard();
    expect(await screen.findByText("Not built yet")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Build profile" }));
    expect(mocked.rebuildDataSourceAiProfile).toHaveBeenCalledWith(7);
    expect(await screen.findByRole("button", { name: /Updating…/ })).toBeDisabled();
  });

  it("a 409 announces the running build without an error", async () => {
    mocked.getDataSourceAiProfile
      .mockResolvedValueOnce(base)
      .mockResolvedValue({ ...base, building: true });
    mocked.rebuildDataSourceAiProfile.mockResolvedValue({ status: "already_queued" });
    const user = userEvent.setup();
    renderCard();
    await user.click(await screen.findByRole("button", { name: "Build profile" }));
    expect(await screen.findByText("A build is already running.")).toBeInTheDocument();
    expect(screen.queryByText(/Couldn't start the rebuild/)).not.toBeInTheDocument();
  });

  it("the 409 note disappears when a refetch reports building:false", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue(base);
    mocked.rebuildDataSourceAiProfile.mockResolvedValue({ status: "already_queued" });
    const user = userEvent.setup();
    renderCard();
    await user.click(await screen.findByRole("button", { name: "Build profile" }));
    await waitFor(() => expect(screen.queryByText("A build is already running.")).not.toBeInTheDocument());
    expect(await screen.findByText("Not built yet")).toBeInTheDocument();
  });

  it("schema already cached at mount does not double-fetch the profile", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue(base);
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["data-source-schema", 7], { schema: { tables: [{ name: "orders", columns: [] }] } });
    renderCard("2026-10-04T00:00:00Z", qc);
    expect(await screen.findByText("Not built yet")).toBeInTheDocument();
    expect(mocked.getDataSourceAiProfile).toHaveBeenCalledTimes(1);
  });

  it("a schema cache update after mount refetches the profile", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue(base);
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["data-source-schema", 7], { schema: { tables: [] } }, { updatedAt: 1000 });
    renderCard("2026-10-04T00:00:00Z", qc);
    expect(await screen.findByText("Not built yet")).toBeInTheDocument();
    act(() => {
      qc.setQueryData(["data-source-schema", 7], { schema: { tables: [{ name: "t", columns: [] }] } });
    });
    await waitFor(() => expect(mocked.getDataSourceAiProfile).toHaveBeenCalledTimes(2));
  });

  it("current:false shows the out-of-date copy", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue({ ...built, current: false });
    renderCard();
    expect(await screen.findByText(/Out of date · some tables aren.t in the profile yet/)).toBeInTheDocument();
  });

  it("supported:false shows the unsupported copy and no button", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue({ ...base, supported: false });
    renderCard();
    expect(
      await screen.findByText(/AI profiles aren't available for PostgreSQL data sources yet/),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });

  it("sample sharing off hides the Samples column and shows the settings link and held-back note", async () => {
    mocked.getAiSettings.mockResolvedValue({ include_sample_values: false });
    mocked.getDataSourceAiProfile.mockResolvedValue(built);
    renderCard();
    expect(await screen.findByRole("link", { name: "Change in AI Settings" })).toHaveAttribute(
      "href",
      "/organization/ai",
    );
    expect(screen.queryByRole("columnheader", { name: "Samples" })).not.toBeInTheDocument();
    expect(screen.getByText(/held back from AI while sample sharing is off/)).toBeInTheDocument();
  });

  it("renders counts, relationship source labels and column cells from the fixture", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue(built);
    renderCard();
    expect(
      await screen.findByText("2 tables · 2 columns · 3 relationships · 1 AI column description"),
    ).toBeInTheDocument();
    expect(screen.getByText("Foreign key")).toBeInTheDocument();
    expect(screen.getByText("From saved queries")).toBeInTheDocument();
    expect(screen.getByText("Matched by name")).toBeInTheDocument();
    expect(screen.getByText("200 rows sampled")).toBeInTheDocument();
    expect(screen.getByText("No sample values")).toBeInTheDocument();
    expect(screen.queryByText("Couldn't sample this table")).not.toBeInTheDocument();
    expect(screen.getByText("26%")).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Samples" })).toBeInTheDocument();
  });

  it("last_error shows the failed banner and Retry calls rebuild; no client-side guess without it", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue({ ...built, last_error: "connection refused" });
    mocked.rebuildDataSourceAiProfile.mockResolvedValue({ status: "queued" });
    const user = userEvent.setup();
    renderCard();
    expect(await screen.findByTestId("ai-profile-failed")).toHaveTextContent(
      "The last profile update failed: connection refused",
    );
    expect(screen.queryByText(/Ready · built/)).not.toBeInTheDocument();
    await user.click(screen.getByTestId("ai-profile-retry"));
    expect(mocked.rebuildDataSourceAiProfile).toHaveBeenCalledWith(7);
    expect(screen.getByRole("heading", { name: "AI profile" }).closest("section")).toHaveAttribute(
      "id",
      "ai-profile",
    );
  });

  it("building with a stale last_error shows no banner; unchanged built_at alone is not a failure", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue({ ...built, building: true, last_error: "old" });
    renderCard();
    expect(await screen.findByText(/^Updating profile… This can take/)).toBeInTheDocument();
    expect(screen.queryByTestId("ai-profile-failed")).not.toBeInTheDocument();
  });

  it("unsampled tables are listed with their error", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue({
      ...built,
      profile: {
        ...profile,
        tables: [
          profile.tables[0],
          { name: "locked", sampled_rows: 0, columns: [{ name: "id", null_frac: 0, distinct_in_sample: 0, samples: [] }], error: "permission denied" },
          profile.tables[1],
        ],
      },
    });
    renderCard();
    const box = await screen.findByTestId("ai-profile-unsampled");
    expect(box).toHaveTextContent("1 of 3 tables couldn't be sampled");
    expect(box).toHaveTextContent("locked — permission denied");
    expect(box).not.toHaveTextContent("empty");
    expect(screen.getAllByText("Couldn't sample this table")).toHaveLength(1);
  });

  it("bare tables without an error are not failures", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue({
      ...built,
      profile: { ...profile, tables: [{ name: "v_orders", sampled_rows: 0, columns: [] }] },
    });
    renderCard();
    expect(await screen.findByText("No sample values")).toBeInTheDocument();
    expect(screen.queryByTestId("ai-profile-unsampled")).not.toBeInTheDocument();
    expect(screen.queryByText("Couldn't sample this table")).not.toBeInTheDocument();
  });

  it("no unsampled notice when every table sampled", async () => {
    mocked.getDataSourceAiProfile.mockResolvedValue({
      ...built,
      profile: { ...profile, tables: [profile.tables[0]] },
    });
    renderCard();
    await screen.findByText(/1 table · 2 columns/);
    expect(screen.queryByTestId("ai-profile-unsampled")).not.toBeInTheDocument();
  });

  describe("metered source", () => {
    const sampled = {
      ...profile,
      tables: [{ ...profile.tables[0], data_profiled_at: "2026-10-03T00:00:00Z" }, { ...profile.tables[0], name: "b", data_profiled_at: "2026-10-04T00:00:00Z" }],
    };
    const meteredBuilt = { ...built, profile: sampled, metered: true };

    it("shows the note and sample age; Cancel sends nothing, confirm sends one rebuild", async () => {
      mocked.getDataSourceAiProfile.mockResolvedValue(meteredBuilt);
      mocked.rebuildDataSourceAiProfile.mockResolvedValue({ status: "queued" });
      const user = userEvent.setup();
      renderCard();
      expect(await screen.findByTestId("ai-profile-metered-note")).toHaveTextContent(
        "PostgreSQL bills for every query",
      );
      expect(screen.getByText(/Ready · sample values from/)).toBeInTheDocument();
      await user.click(screen.getByRole("button", { name: "Rebuild profile" }));
      expect(await screen.findByTestId("ai-profile-metered-confirm")).toHaveTextContent(
        "each of the 2 tables",
      );
      await user.click(screen.getByRole("button", { name: "Cancel" }));
      expect(mocked.rebuildDataSourceAiProfile).not.toHaveBeenCalled();
      await user.click(screen.getByRole("button", { name: "Rebuild profile" }));
      await user.click(await screen.findByRole("button", { name: "Run sample queries" }));
      await waitFor(() => expect(mocked.rebuildDataSourceAiProfile).toHaveBeenCalledTimes(1));
      await waitFor(() => expect(screen.queryByTestId("ai-profile-metered-confirm")).not.toBeInTheDocument());
    });

    it("no samples yet: status and Collect sample values button", async () => {
      const unsampled = { ...profile, tables: [{ ...profile.tables[0], sampled_rows: 0 }] };
      mocked.getDataSourceAiProfile.mockResolvedValue({ ...built, profile: unsampled, metered: true });
      renderCard();
      expect(await screen.findByText("Sample values not collected yet")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Collect sample values" })).toBeInTheDocument();
    });

    it("legacy sampled profile without per-table timestamps counts as collected", async () => {
      mocked.getDataSourceAiProfile.mockResolvedValue({ ...built, metered: true });
      renderCard();
      expect(await screen.findByText(/Ready · sample values from/)).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Rebuild profile" })).toBeInTheDocument();
    });

    it("non-metered Rebuild opens no dialog", async () => {
      mocked.getDataSourceAiProfile.mockResolvedValue(built);
      mocked.rebuildDataSourceAiProfile.mockResolvedValue({ status: "queued" });
      const user = userEvent.setup();
      renderCard();
      await user.click(await screen.findByRole("button", { name: "Rebuild profile" }));
      expect(screen.queryByTestId("ai-profile-metered-confirm")).not.toBeInTheDocument();
      expect(screen.queryByTestId("ai-profile-metered-note")).not.toBeInTheDocument();
      expect(mocked.rebuildDataSourceAiProfile).toHaveBeenCalledTimes(1);
    });
  });
});
