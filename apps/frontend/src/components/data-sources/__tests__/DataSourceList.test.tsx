import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { DataSourceList, HealthIndicator } from "../DataSourceList";
import { ApiError } from "@/lib/api";
import type * as ApiModule from "@/lib/api";
import type { DataSource, DataSourceType } from "@/types/api";

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      deleteDataSource: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";
import { usePermissions } from "@/lib/permissions";

vi.mock("@/lib/permissions", () => ({ usePermissions: vi.fn(() => ({ isAdmin: false })) }));
const mockedPerms = usePermissions as unknown as ReturnType<typeof vi.fn>;

const mockedApi = api as unknown as { deleteDataSource: ReturnType<typeof vi.fn> };

const DS: DataSource = {
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
};

const TYPES: DataSourceType[] = [
  { type: "pg", name: "PostgreSQL", configuration_schema: {} },
];

function renderList(ds: DataSource = DS) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <DataSourceList dataSources={[ds]} types={TYPES} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("DataSourceList delete flow", () => {
  it("describes the real refusal behavior instead of claiming queries will break", async () => {
    const user = userEvent.setup();
    renderList();

    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).getByText(/data sources still used by a query can't be deleted/i),
    ).toBeInTheDocument();
    expect(
      within(dialog).queryByText(/any queries using it will stop working/i),
    ).not.toBeInTheDocument();
  });

  it("surfaces the backend's conflict message instead of leaving the dialog silently stuck", async () => {
    mockedApi.deleteDataSource.mockRejectedValue(
      new ApiError(409, "3 queries still use this data source"),
    );
    const user = userEvent.setup();
    renderList();

    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Delete" }));

    await waitFor(() =>
      expect(within(dialog).getByText("3 queries still use this data source")).toBeInTheDocument(),
    );
    // The error must render inside the still-open dialog, not behind its overlay.
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });
});

// Each of the four health states must differ by visible text (never colour alone —
// WCAG 1.4.1), and a successful test/query must be distinguishable from "never checked".
describe("HealthIndicator", () => {
  it("Off: paused, hollow ring, no health wording", () => {
    render(<HealthIndicator ds={{ ...DS, paused: true, pause_reason: "Maintenance" }} />);
    expect(screen.getByText("Off")).toBeInTheDocument();
  });

  it("On · Healthy: health_state ok", () => {
    render(<HealthIndicator ds={{ ...DS, health_state: "ok", health_checked_at: "2026-01-01T00:00:00Z" }} />);
    expect(screen.getByText("On · Healthy")).toBeInTheDocument();
  });

  it("On · Not checked yet: health_state unknown (never used, never tested)", () => {
    render(<HealthIndicator ds={{ ...DS, health_state: "unknown" }} />);
    expect(screen.getByText("On · Not checked yet")).toBeInTheDocument();
  });

  it("On · Unreachable: health_state failing, distinct text and an icon (not just a red dot)", () => {
    const { container } = render(
      <HealthIndicator ds={{ ...DS, health_state: "failing", health_error: "connection refused" }} />,
    );
    expect(screen.getByText("On · Unreachable")).toBeInTheDocument();
    expect(container.querySelector("svg")).toBeInTheDocument();
  });

  it("every non-Off state renders distinct text from every other state", () => {
    const states: DataSource["health_state"][] = ["ok", "unknown", "failing"];
    const texts = states.map((health_state) => {
      const { container, unmount } = render(<HealthIndicator ds={{ ...DS, health_state }} />);
      const text = container.textContent;
      unmount();
      return text;
    });
    expect(new Set(texts).size).toBe(states.length);
  });
});

describe("DataSourceList profile chip", () => {
  it("shows Profiling… for admins, and profiling wins over failed", () => {
    mockedPerms.mockReturnValue({ isAdmin: true });
    renderList({ ...DS, profiling: true, profile_failed: true });
    const chip = screen.getByTestId("ds-profile-chip");
    expect(chip).toHaveAttribute("data-state", "profiling");
    expect(chip).toHaveTextContent("Profiling…");
  });

  it("shows a failed chip linking to the profile card", () => {
    mockedPerms.mockReturnValue({ isAdmin: true });
    renderList({ ...DS, profile_failed: true });
    const chip = screen.getByTestId("ds-profile-chip");
    expect(chip).toHaveAttribute("data-state", "failed");
    expect(chip.closest("a")).toHaveAttribute("href", "/data_sources/1#ai-profile");
  });

  it("renders no chip when neither flag is set, or for non-admins", () => {
    mockedPerms.mockReturnValue({ isAdmin: true });
    const { unmount } = renderList(DS);
    expect(screen.queryByTestId("ds-profile-chip")).toBeNull();
    unmount();
    mockedPerms.mockReturnValue({ isAdmin: false });
    renderList({ ...DS, profiling: true });
    expect(screen.queryByTestId("ds-profile-chip")).toBeNull();
  });
});
