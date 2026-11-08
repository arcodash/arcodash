import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router";
import type * as RouterModule from "react-router";
import { AddMonitorWithAiDialog } from "../AddMonitorWithAiDialog";
import { api } from "@/lib/api";
import type { AskEvent } from "@/types/api";

vi.mock("@/lib/permissions", () => ({ usePermissions: () => ({ has: () => true, hasAny: () => true }) }));
const navigate = vi.fn();
vi.mock("react-router", async (orig) => ({ ...(await orig<typeof RouterModule>()), useNavigate: () => navigate }));

const ds = (id: number) => ({ id, name: `DS${id}`, type: "pg", type_display_name: "PG" });
const suggestion = {
  name: "Signups drop",
  kind: "baseline",
  column: "signups",
  sensitivity: "medium",
  op: null,
  value: null,
  definition: "d",
  why: "w",
  confidence: "high",
};

function setup(events: AskEvent[] | Error) {
  vi.spyOn(api, "listDataSources").mockResolvedValue({ data_sources: [ds(3), ds(4)], can_create: true } as never);
  const stream = vi.spyOn(api, "askMonitorStream").mockImplementation(async (_b, onEvent) => {
    if (events instanceof Error) throw events;
    events.forEach(onEvent);
    return new Promise<void>(() => {});
  });
  const onOpenChange = vi.fn();
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <AddMonitorWithAiDialog open onOpenChange={onOpenChange} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return { stream, onOpenChange };
}

async function submitPrompt(text: string) {
  const user = userEvent.setup();
  await user.type(await screen.findByLabelText("Describe what to monitor"), text);
  await user.click(screen.getByTestId("monitor-ask-submit"));
  return user;
}

describe("AddMonitorWithAiDialog", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    navigate.mockReset();
  });

  it("starts the stream with the default source and trimmed prompt", async () => {
    const { stream } = setup([{ type: "status", text: "Generating SQL" }]);
    await submitPrompt("  daily signups  ");
    await waitFor(() => expect(stream).toHaveBeenCalled());
    expect(stream.mock.calls[0][0]).toEqual({ data_source_id: 3, prompt: "daily signups" });
  });

  it("hands off to /monitors/new with the draft and preview columns on suggested", async () => {
    const columns = [{ name: "signups", friendly_type: "integer" }];
    const { onOpenChange } = setup([
      { type: "preview", attempt: 1, columns, rows: [[5]], row_count: 1, truncated: false },
      { type: "suggesting" },
      { type: "suggested", sql: "SELECT 1 AS signups", data_source_id: 3, suggestion } as AskEvent,
    ]);
    await submitPrompt("daily signups");
    await waitFor(() => expect(navigate).toHaveBeenCalled());
    expect(navigate).toHaveBeenCalledWith("/monitors/new", {
      state: {
        aiDraft: {
          sql: "SELECT 1 AS signups",
          data_source_id: 3,
          query_name: "daily signups",
          suggestion,
          columns,
        },
      },
    });
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it("shows the server reason and Open in editor on failure", async () => {
    setup([{ type: "failed", status: "generation_failed", message: "no valid SQL" } as AskEvent]);
    const user = await submitPrompt("daily signups");
    expect(await screen.findByText(/no valid SQL/)).toBeTruthy();
    await user.click(screen.getByTestId("monitor-ask-open-editor"));
    expect(navigate).toHaveBeenCalledWith("/queries/new", {
      state: { askQuestion: "daily signups", dataSourceId: 3 },
    });
    expect(screen.getByRole("button", { name: "Try again" })).toBeTruthy();
  });
});
