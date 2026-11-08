import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, within, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { VizPanel } from "../VizPanel";
import { vizRegistry } from "../index";
import { counterDefaultOptions } from "../counter";
import type * as ApiModule from "@/lib/api";
import type { Visualization } from "@/types/api";
import type { ResultData } from "@/types/api";

vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof ApiModule>("@/lib/api");
  return {
    ...actual,
    api: {
      listVisualizations: vi.fn(),
      createVisualization: vi.fn(),
      updateVisualization: vi.fn(),
      deleteVisualization: vi.fn(),
      countActivePublicEmbeds: vi.fn(),
    },
  };
});

import { api, ApiError } from "@/lib/api";

const mockedApi = api as unknown as {
  listVisualizations: ReturnType<typeof vi.fn>;
  createVisualization: ReturnType<typeof vi.fn>;
  updateVisualization: ReturnType<typeof vi.fn>;
  deleteVisualization: ReturnType<typeof vi.fn>;
  countActivePublicEmbeds: ReturnType<typeof vi.fn>;
};

const result: ResultData = {
  columns: [{ name: "total", friendly_type: "integer" }],
  rows: [{ total: 42 }],
};

let nextId = 100;

function makeViz(overrides: Partial<Visualization>): Visualization {
  return {
    id: 1,
    query_id: 7,
    type: "TABLE",
    name: "My Table",
    options: {},
    created_at: "2024-01-01T00:00:00Z",
    updated_at: "2024-01-01T00:00:00Z",
    ...overrides,
  };
}

type AutoCreateRequest = { type: string; name: string; options: Record<string, unknown> } | null;

/** Wires the mocked api to a live mutable list so mutations show up on the next invalidation
 * refetch; `rerenderWith` keeps the same QueryClient across auto-create re-renders. */
function renderPanel(
  initialVizs: Visualization[],
  autoProps?: { autoCreateRequest?: AutoCreateRequest; onAutoCreateHandled?: () => void },
) {
  const vizs = [...initialVizs];
  mockedApi.listVisualizations.mockImplementation(() => Promise.resolve([...vizs]));
  // Default only when the test hasn't configured its own response; those must win.
  if (mockedApi.countActivePublicEmbeds.getMockImplementation() === undefined) {
    mockedApi.countActivePublicEmbeds.mockImplementation(() => Promise.resolve({ count: 0 }));
  }
  mockedApi.createVisualization.mockImplementation(({ type, name, options }) => {
    const viz = makeViz({ id: ++nextId, type, name, options });
    vizs.push(viz);
    return Promise.resolve(viz);
  });
  mockedApi.updateVisualization.mockImplementation((id: number, patch: Partial<Visualization>) => {
    const viz = vizs.find((v) => v.id === id)!;
    Object.assign(viz, patch);
    return Promise.resolve(viz);
  });
  mockedApi.deleteVisualization.mockImplementation((id: number) => {
    const idx = vizs.findIndex((v) => v.id === id);
    if (idx >= 0) vizs.splice(idx, 1);
    return Promise.resolve(undefined);
  });

  // Matches App.tsx's staleTime; a test-only 0 would hide a merge reading a stale cached row.
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: 30_000 } },
  });
  function tree(props?: { autoCreateRequest?: AutoCreateRequest; onAutoCreateHandled?: () => void }) {
    return (
      <QueryClientProvider client={qc}>
        <VizPanel
          queryId={7}
          result={result}
          autoCreateRequest={props?.autoCreateRequest}
          onAutoCreateHandled={props?.onAutoCreateHandled}
        />
      </QueryClientProvider>
    );
  }
  const view = render(tree(autoProps));
  return {
    ...view,
    rerenderWith: (props?: { autoCreateRequest?: AutoCreateRequest; onAutoCreateHandled?: () => void }) =>
      view.rerender(tree(props)),
  };
}

async function openSelect(user: ReturnType<typeof userEvent.setup>, trigger: HTMLElement) {
  await user.click(trigger);
  return screen.findByRole("listbox");
}

beforeEach(() => {
  vi.clearAllMocks();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("VizPanel", () => {
  it("renders one tab per visualization, plus Results", async () => {
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" }), makeViz({ id: 2, type: "COUNTER", name: "My Counter" })]);

    expect(await screen.findByRole("button", { name: "My Table" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "My Counter" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Results" })).toBeInTheDocument();
  });

  it("Add Visualization dialog lists every registry type", async () => {
    const user = userEvent.setup();
    renderPanel([]);
    await screen.findByRole("button", { name: "Results" });

    await user.click(screen.getByRole("button", { name: /add visualization/i }));
    const dialog = await screen.findByRole("dialog");
    const listbox = await openSelect(user, within(dialog).getByRole("combobox"));
    const optionNames = within(listbox).getAllByRole("option").map((o) => o.textContent);

    expect(vizRegistry).toHaveLength(4);
    expect(optionNames).toEqual(vizRegistry.map((d) => d.name));
  });

  it("Add is disabled with a reason until a name is entered, and Name is marked required", async () => {
    const user = userEvent.setup();
    renderPanel([]);
    await screen.findByRole("button", { name: "Results" });

    await user.click(screen.getByRole("button", { name: /add visualization/i }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Name").parentElement).toHaveTextContent("Name *");

    const addButton = within(dialog).getByRole("button", { name: "Add" });
    expect(addButton).toBeDisabled();
    expect(addButton).toHaveAttribute("title", "Enter a name");
    expect(getComputedStyle(addButton).pointerEvents).not.toBe("none");

    await user.type(within(dialog).getByPlaceholderText("My visualization"), "New chart");
    expect(addButton).toBeEnabled();
    expect(addButton).not.toHaveAttribute("title");
  });

  it("creating a visualization sends the selected type's defaultOptions", async () => {
    const user = userEvent.setup();
    renderPanel([]);
    await screen.findByRole("button", { name: "Results" });

    await user.click(screen.getByRole("button", { name: /add visualization/i }));
    const dialog = await screen.findByRole("dialog");
    const listbox = await openSelect(user, within(dialog).getByRole("combobox"));
    await user.click(within(listbox).getByText("Counter"));
    await user.type(within(dialog).getByPlaceholderText("My visualization"), "New Counter");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    await waitFor(() =>
      expect(mockedApi.createVisualization).toHaveBeenCalledWith({
        query_id: 7,
        type: "COUNTER",
        name: "New Counter",
        options: counterDefaultOptions,
      }),
    );
    expect(await screen.findByRole("button", { name: "New Counter" })).toBeInTheDocument();
  });

  it("keeps the Add Visualization dialog open with the typed name intact when create fails", async () => {
    const user = userEvent.setup();
    renderPanel([]);
    await screen.findByRole("button", { name: "Results" });
    mockedApi.createVisualization.mockRejectedValue(new ApiError(500, "database unavailable"));

    await user.click(screen.getByRole("button", { name: /add visualization/i }));
    const dialog = await screen.findByRole("dialog");
    await user.type(within(dialog).getByPlaceholderText("My visualization"), "New chart");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    expect(await within(dialog).findByText("database unavailable")).toBeInTheDocument();
    expect(within(dialog).getByPlaceholderText("My visualization")).toHaveValue("New chart");
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("switches to the new tab once creation succeeds", async () => {
    const user = userEvent.setup();
    renderPanel([]);
    await screen.findByRole("button", { name: "Results" });

    await user.click(screen.getByRole("button", { name: /add visualization/i }));
    const dialog = await screen.findByRole("dialog");
    const listbox = await openSelect(user, within(dialog).getByRole("combobox"));
    await user.click(within(listbox).getByText("Counter"));
    await user.type(within(dialog).getByPlaceholderText("My visualization"), "New Counter");
    await user.click(within(dialog).getByRole("button", { name: "Add" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(await screen.findByRole("button", { name: "New Counter" })).toBeInTheDocument();
  });

  it("switching tabs renders the matching visualization's renderer", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    expect(await screen.findByText("42")).toBeInTheDocument();
  });

  it("saving an edit calls updateVisualization(id, { options })", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Edit visualization"));

    const dialog = await screen.findByRole("dialog");
    const itemsPerPage = within(dialog).getByRole("spinbutton");
    await user.clear(itemsPerPage);
    await user.type(itemsPerPage, "25");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(mockedApi.updateVisualization).toHaveBeenCalledWith(1, {
        options: { itemsPerPage: 25 },
      }),
    );
  });

  it("renaming sends the trimmed name alongside options", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Edit visualization"));
    const dialog = await screen.findByRole("dialog");
    const nameInput = within(dialog).getByLabelText(/^Name/);
    await user.clear(nameInput);
    await user.type(nameInput, "  Renamed  ");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(mockedApi.updateVisualization).toHaveBeenCalledWith(1, {
        name: "Renamed",
        options: {},
      }),
    );
  });

  it("does not send name when it is unchanged", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Edit visualization"));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    await waitFor(() => expect(mockedApi.updateVisualization).toHaveBeenCalled());
    expect(mockedApi.updateVisualization.mock.calls[0][1]).not.toHaveProperty("name");
  });

  it("disables Save when the name is blank", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Edit visualization"));
    const dialog = await screen.findByRole("dialog");
    await user.clear(within(dialog).getByLabelText(/^Name/));
    await user.type(within(dialog).getByLabelText(/^Name/), "   ");

    expect(within(dialog).getByRole("button", { name: "Save" })).toBeDisabled();
  });

  it("a name-only edit followed by Cancel asks to discard unsaved changes", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Edit visualization"));
    const dialog = await screen.findByRole("dialog");
    await user.type(within(dialog).getByLabelText(/^Name/), "x");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

    expect(await screen.findByText("Discard unsaved changes?")).toBeInTheDocument();
    expect(mockedApi.updateVisualization).not.toHaveBeenCalled();
  });

  it("a whitespace-only name edit followed by Cancel closes without the discard confirm", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Edit visualization"));
    const dialog = await screen.findByRole("dialog");
    await user.type(within(dialog).getByLabelText(/^Name/), "   ");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.queryByText("Discard unsaved changes?")).not.toBeInTheDocument();
  });

  it("merges onto a freshly re-read row instead of reverting a key a concurrent editor changed", async () => {
    const user = userEvent.setup();
    renderPanel([
      makeViz({
        id: 1,
        type: "COUNTER",
        name: "My Counter",
        options: { stringPrefix: "a", stringSuffix: "b" },
      }),
    ]);

    await user.click(await screen.findByRole("button", { name: "My Counter" }));
    await user.click(screen.getByTitle("Edit visualization"));
    const dialog = await screen.findByRole("dialog");

    // Concurrent editor changes a key this dialog never touches; the panel's re-fetch on save picks
    // it up.
    mockedApi.listVisualizations.mockResolvedValue([
      makeViz({
        id: 1,
        type: "COUNTER",
        name: "My Counter",
        options: { stringPrefix: "a", stringSuffix: "CHANGED" },
      }),
    ]);

    const prefixInput = within(dialog).getByDisplayValue("a");
    await user.clear(prefixInput);
    await user.type(prefixInput, "new-prefix");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(mockedApi.updateVisualization).toHaveBeenCalledWith(1, {
        options: { stringPrefix: "new-prefix", stringSuffix: "CHANGED" },
      }),
    );
  });

  it("shows an inline error when saving an edit fails, instead of failing silently", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);
    mockedApi.updateVisualization.mockRejectedValue(new ApiError(500, "database unavailable"));

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Edit visualization"));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });

  it("clicking delete opens the confirm dialog and does not call the API", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Delete visualization"));

    expect(await screen.findByText("Delete this visualization?")).toBeInTheDocument();
    expect(mockedApi.deleteVisualization).not.toHaveBeenCalled();
  });

  it("confirming the delete dialog calls deleteVisualization and closes the dialog", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Delete visualization"));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: /^delete$/i }));

    await waitFor(() => expect(mockedApi.deleteVisualization).toHaveBeenCalledWith(1));
    await waitFor(() => expect(screen.queryByRole("button", { name: "My Table" })).not.toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Results" })).toHaveClass("border-primary");
    expect(screen.queryByText("Delete this visualization?")).not.toBeInTheDocument();
  });

  it("cancelling the delete dialog calls nothing", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Delete visualization"));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: /^cancel$/i }));

    expect(mockedApi.deleteVisualization).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "My Table" })).toBeInTheDocument();
  });

  it("shows an inline error and keeps the confirm dialog open when deleting fails, instead of failing silently", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "TABLE", name: "My Table" })]);
    mockedApi.deleteVisualization.mockRejectedValue(new ApiError(500, "database unavailable"));

    await user.click(await screen.findByRole("button", { name: "My Table" }));
    await user.click(screen.getByTitle("Delete visualization"));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: /^delete$/i }));

    expect(await within(dialog).findByText("database unavailable")).toBeInTheDocument();
    // Query with hidden:true: Radix marks the background aria-hidden while the dialog is open.
    expect(screen.getByRole("button", { name: "My Table", hidden: true })).toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("renders a fallback message for an unknown visualization type", async () => {
    const user = userEvent.setup();
    renderPanel([makeViz({ id: 1, type: "NOT_A_REAL_TYPE", name: "Mystery" })]);

    await user.click(await screen.findByRole("button", { name: "Mystery" }));
    expect(await screen.findByText(/unknown visualization type/i)).toBeInTheDocument();
  });

  describe("active-embed warning", () => {
    it("shows the active-embed count once the check succeeds", async () => {
      mockedApi.countActivePublicEmbeds.mockResolvedValue({ count: 2 });
      renderPanel([]);

      expect(await screen.findByText(/active public embeds/i)).toBeInTheDocument();
    });

    it("shows nothing once the check succeeds and finds no active embeds", async () => {
      mockedApi.countActivePublicEmbeds.mockResolvedValue({ count: 0 });
      renderPanel([]);
      await screen.findByRole("button", { name: "Results" });

      expect(screen.queryByText(/active public embed/i)).not.toBeInTheDocument();
    });

    it("a failed embed-count query still warns that embeds may exist, instead of silently disappearing", async () => {
      mockedApi.countActivePublicEmbeds.mockRejectedValue(new Error("network error"));
      renderPanel([]);

      expect(
        await screen.findByText(/couldn.t check whether this query has active public embeds/i),
      ).toBeInTheDocument();
    });
  });

  describe("Auto Visualize handoff", () => {
    const AUTO_CHART = {
      type: "CHART",
      name: "✨ Status breakdown",
      options: { globalSeriesType: "bar" },
    };

    it("an autoCreateRequest creates with the options used verbatim, then reports handled", async () => {
      const onAutoCreateHandled = vi.fn();
      renderPanel([], { autoCreateRequest: AUTO_CHART, onAutoCreateHandled });
      await screen.findByRole("button", { name: "Results" });

      await waitFor(() =>
        expect(mockedApi.createVisualization).toHaveBeenCalledWith({
          query_id: 7,
          type: "CHART",
          name: "✨ Status breakdown",
          options: { globalSeriesType: "bar" },
        }),
      );
      expect(mockedApi.createVisualization).toHaveBeenCalledTimes(1);
      expect(onAutoCreateHandled).toHaveBeenCalledTimes(1);
    });

    it("re-rendering with the same request object (or clearing it to null) never re-fires create", async () => {
      const onAutoCreateHandled = vi.fn();
      const view = renderPanel([], { autoCreateRequest: AUTO_CHART, onAutoCreateHandled });
      await waitFor(() => expect(mockedApi.createVisualization).toHaveBeenCalledTimes(1));

      view.rerenderWith({ autoCreateRequest: AUTO_CHART, onAutoCreateHandled });
      view.rerenderWith({ autoCreateRequest: null, onAutoCreateHandled });

      expect(mockedApi.createVisualization).toHaveBeenCalledTimes(1);
    });

    it("a new request object (different identity) fires a second create with the new payload", async () => {
      const onAutoCreateHandled = vi.fn();
      const view = renderPanel([], { autoCreateRequest: AUTO_CHART, onAutoCreateHandled });
      await waitFor(() => expect(mockedApi.createVisualization).toHaveBeenCalledTimes(1));

      const AUTO_COUNTER = { type: "COUNTER", name: "✨ Total", options: { columnName: "total" } };
      view.rerenderWith({ autoCreateRequest: AUTO_COUNTER, onAutoCreateHandled });

      await waitFor(() =>
        expect(mockedApi.createVisualization).toHaveBeenNthCalledWith(2, {
          query_id: 7,
          type: "COUNTER",
          name: "✨ Total",
          options: { columnName: "total" },
        }),
      );
      expect(mockedApi.createVisualization).toHaveBeenCalledTimes(2);
    });

    it("after the auto-created viz succeeds, its tab appears and is active", async () => {
      renderPanel([], { autoCreateRequest: AUTO_CHART, onAutoCreateHandled: vi.fn() });

      const tab = await screen.findByRole("button", { name: "✨ Status breakdown" });
      expect(tab).toHaveClass("border-primary");
    });

    it("a failed auto-create surfaces an inline error — this fires with no dialog open, so it must be visible on the page itself", async () => {
      const view = renderPanel([], { autoCreateRequest: AUTO_CHART, onAutoCreateHandled: vi.fn() });
      await waitFor(() => expect(mockedApi.createVisualization).toHaveBeenCalledTimes(1));

      mockedApi.createVisualization.mockRejectedValue(new ApiError(500, "database unavailable"));
      view.rerenderWith({ autoCreateRequest: { ...AUTO_CHART }, onAutoCreateHandled: vi.fn() });

      expect(await screen.findByText("database unavailable")).toBeInTheDocument();
    });
  });
});
