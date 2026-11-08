import type { ComponentProps } from "react";
import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { VersionHistoryPanel } from "../VersionHistoryPanel";
import { ConflictError } from "@/lib/api";
import type { QueryVersion } from "@/types/api";

vi.mock("@/lib/api", () => {
  class ApiError extends Error {
    status: number;
    constructor(status: number, message: string) {
      super(message);
      this.status = status;
    }
  }
  class ConflictError extends ApiError {
    workingVersion: number;
    constructor(message: string, workingVersion: number) {
      super(409, message);
      this.workingVersion = workingVersion;
    }
  }
  return {
    api: {
      getQueryVersions: vi.fn(),
      revertQuery: vi.fn(),
      restoreQueryVersion: vi.fn(),
    },
    ApiError,
    ConflictError,
  };
});

import { api } from "@/lib/api";

const mockedApi = api as unknown as {
  getQueryVersions: ReturnType<typeof vi.fn>;
  revertQuery: ReturnType<typeof vi.fn>;
  restoreQueryVersion: ReturnType<typeof vi.fn>;
};

const VERSIONS: QueryVersion[] = [
  {
    change_id: 1,
    version: 1,
    user_id: 9,
    user_name: "Ada Lovelace",
    created_at: "2025-01-01T00:00:00Z",
    change: { name: "v1", description: "", query: "SELECT 1", data_source_id: 1, options: {}, is_archived: false },
  },
  {
    change_id: 2,
    version: 2,
    user_id: 146,
    user_name: null,
    created_at: "2025-01-02T00:00:00Z",
    change: { name: "v2", description: "", query: "SELECT 2", data_source_id: 1, options: {}, is_archived: false },
  },
];

function renderPanel(props: Partial<ComponentProps<typeof VersionHistoryPanel>> = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <VersionHistoryPanel
        queryId={42}
        workingVersion={0}
        isCurrentlyPublished={true}
        onReverted={vi.fn()}
        onConflict={vi.fn()}
        open={false}
        onOpenChange={vi.fn()}
        {...props}
      />
    </QueryClientProvider>,
  );
}

describe("VersionHistoryPanel (controlled Dialog)", () => {
  beforeEach(() => {
    mockedApi.getQueryVersions.mockReset().mockResolvedValue(VERSIONS);
    mockedApi.revertQuery.mockReset().mockResolvedValue({});
  });

  it("does not fetch versions while closed", () => {
    renderPanel({ open: false });
    expect(mockedApi.getQueryVersions).not.toHaveBeenCalled();
  });

  it("fetches and renders versions newest-first when open", async () => {
    renderPanel({ open: true });
    expect(mockedApi.getQueryVersions).toHaveBeenCalledWith(42);
    expect(await screen.findByText("Version history")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByText("2 version(s)")).toBeInTheDocument());
    expect(screen.getByText(/Version 2/)).toBeInTheDocument();
    expect(screen.getByText(/Version 1/)).toBeInTheDocument();
    expect(screen.getAllByTitle("Load into the draft (doesn't change what's published)")).toHaveLength(1);
    expect(screen.getAllByTitle("Restore — make this version the published version")).toHaveLength(1);
    expect(screen.getByText(/Ada Lovelace/)).toBeInTheDocument();
    expect(screen.getByText(/User #146/)).toBeInTheDocument();
  });

  it("states that only published versions are kept, with a link to the docs", async () => {
    renderPanel({ open: true });
    expect(
      await screen.findByText(/only published versions are kept/i),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: /learn more about version history/i }),
    ).toHaveAttribute(
      "href",
      "https://www.arcodash.com/docs/queries-visualization/versioning-drafts",
    );
  });

  it("reverting calls the API and onReverted", async () => {
    const user = userEvent.setup();
    const onReverted = vi.fn();
    renderPanel({ open: true, onReverted });

    await screen.findByText(/Version 1/);
    await user.click(screen.getByTitle("Load into the draft (doesn't change what's published)"));

    await waitFor(() => expect(mockedApi.revertQuery).toHaveBeenCalledWith(42, { change_id: 1 }));
    await waitFor(() => expect(onReverted).toHaveBeenCalled());
  });

  it("restoring calls the API and onReverted", async () => {
    const user = userEvent.setup();
    const onReverted = vi.fn();
    renderPanel({ open: true, onReverted, workingVersion: 3 });

    await screen.findByText(/Version 1/);
    await user.click(screen.getByTitle("Restore — make this version the published version"));

    await waitFor(() => expect(mockedApi.restoreQueryVersion).toHaveBeenCalledWith(42, 1, 3));
    await waitFor(() => expect(onReverted).toHaveBeenCalled());
  });

  it("calls out that restore republishes when the query is currently unpublished", async () => {
    renderPanel({ open: true, isCurrentlyPublished: false });

    await screen.findByText(/Version 1/);
    expect(
      screen.getByTitle("Restore — publishes this version, even though the query is currently unpublished"),
    ).toBeInTheDocument();
  });

  it("hands a 409 from restore to onConflict instead of the inline error", async () => {
    const user = userEvent.setup();
    const onConflict = vi.fn();
    const conflict = new ConflictError("stale", 9);
    mockedApi.restoreQueryVersion.mockRejectedValueOnce(conflict);
    renderPanel({ open: true, onConflict, workingVersion: 3 });

    await screen.findByText(/Version 1/);
    await user.click(screen.getByTitle("Restore — make this version the published version"));

    await waitFor(() => expect(onConflict).toHaveBeenCalledWith(conflict));
    expect(screen.queryByText("Failed to restore this version")).not.toBeInTheDocument();
  });
});
