import { describe, it, expect, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router";
import { AddWithAiBar, pickDefaultSourceId } from "../AddWithAiBar";
import { api, ApiError } from "@/lib/api";
import type { AskEvent, Widget } from "@/types/api";

vi.mock("@/lib/permissions", () => ({ usePermissions: () => ({ has: () => true, hasAny: () => true }) }));

const ds = (id: number) => ({ id, name: `DS${id}`, type: "pg", type_display_name: "PG" });
const widget = (id: number, sourceId?: number) =>
  ({
    id,
    visualization: sourceId == null ? undefined : { query: { data_source_id: sourceId } },
  }) as unknown as Widget;

function renderBar(sources = [ds(1), ds(2)], widgets: Widget[] = [], onCreated = vi.fn()) {
  vi.spyOn(api, "listDataSources").mockResolvedValue({ data_sources: sources, can_create: true } as never);
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onClose = vi.fn();
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <AddWithAiBar dashboardId={7} widgets={widgets} onClose={onClose} onCreated={onCreated} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return { onClose, onCreated };
}

const promptBox = () => screen.findByLabelText("Describe the widget you want");

describe("pickDefaultSourceId", () => {
  it("prefers the most-used source", () => {
    expect(pickDefaultSourceId([widget(1, 1), widget(2, 2), widget(3, 2)], [1, 2])).toBe(2);
  });
  it("breaks ties by widget order", () => {
    expect(pickDefaultSourceId([widget(1, 2), widget(2, 1)], [1, 2])).toBe(2);
  });
  it("falls back to the first source without query widgets", () => {
    expect(pickDefaultSourceId([widget(1)], [5, 6])).toBe(5);
  });
});

describe("AddWithAiBar", () => {
  beforeEach(() => vi.restoreAllMocks());

  it("hides the source select with a single source", async () => {
    renderBar([ds(1)]);
    await promptBox();
    expect(screen.queryByLabelText("Data source")).toBeNull();
  });

  it("shows the source select with several sources", async () => {
    renderBar();
    expect(await screen.findByLabelText("Data source")).toBeTruthy();
  });

  it("submits on Enter but not during IME composition, and locks while running", async () => {
    const spy = vi.spyOn(api, "askWidgetStream").mockImplementation(() => new Promise(() => {}));
    renderBar([ds(1)]);
    const input = await promptBox();
    await userEvent.type(input, "signups by country");
    fireEvent.keyDown(input, { key: "Enter", isComposing: true });
    expect(spy).not.toHaveBeenCalled();
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1));
    expect(spy.mock.calls[0][1]).toEqual({ data_source_id: 1, prompt: "signups by country" });
    expect((input as HTMLInputElement).disabled).toBe(true);
    expect((screen.getByLabelText("Close") as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByRole("button", { name: "Add" })).toBeNull();
    fireEvent.keyDown(input, { key: "Escape" });
  });

  it("Escape closes when idle", async () => {
    const { onClose } = renderBar([ds(1)]);
    fireEvent.keyDown(await promptBox(), { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("Escape does nothing while running", async () => {
    vi.spyOn(api, "askWidgetStream").mockImplementation(() => new Promise(() => {}));
    const { onClose } = renderBar([ds(1)]);
    const input = await promptBox();
    await userEvent.type(input, "x{Enter}");
    await waitFor(() => expect((input as HTMLInputElement).disabled).toBe(true));
    fireEvent.keyDown(input, { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("failure keeps the prompt and offers Try again and Open in editor", async () => {
    vi.spyOn(api, "askWidgetStream").mockImplementation(async (_i, _b, onEvent) =>
      onEvent({ type: "failed", status: "failed", message: "boom" } as AskEvent));
    renderBar([ds(1)]);
    const input = await promptBox();
    await userEvent.type(input, "chart{Enter}");
    expect(await screen.findByText("Couldn't add a widget")).toBeTruthy();
    expect(screen.getByText("boom")).toBeTruthy();
    expect((input as HTMLInputElement).value).toBe("chart");
    expect(screen.getByRole("button", { name: "Try again" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Open in editor" })).toBeTruthy();
  });

  it("budget exceeded hides both actions", async () => {
    vi.spyOn(api, "askWidgetStream").mockRejectedValue(new ApiError(429, "cap hit", "ai_budget_exceeded"));
    renderBar([ds(1)]);
    await userEvent.type(await promptBox(), "chart{Enter}");
    expect(await screen.findByText(/reached its monthly AI budget/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Open in editor" })).toBeNull();
    expect(screen.getByText("Raise it in AI Settings")).toBeTruthy();
  });

  it("bare 403 shows the server message, hides Try again but keeps Open in editor", async () => {
    vi.spyOn(api, "askWidgetStream").mockRejectedValue(new ApiError(403, "no plan here"));
    renderBar([ds(1)]);
    await userEvent.type(await promptBox(), "chart{Enter}");
    expect(await screen.findByText("no plan here")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    expect(screen.getByRole("button", { name: "Open in editor" })).toBeTruthy();
  });

  it("reports created once", async () => {
    vi.spyOn(api, "askWidgetStream").mockImplementation(async (_i, _b, onEvent) => {
      onEvent({ type: "created", query_id: 1, visualization_id: 2, widget_id: 3, viz_type: "CHART", row_count: 4 });
    });
    const { onCreated } = renderBar([ds(1)]);
    await userEvent.type(await promptBox(), "chart{Enter}");
    await waitFor(() => expect(onCreated).toHaveBeenCalledTimes(1));
    expect(onCreated.mock.calls[0][0].widget_id).toBe(3);
  });
});
