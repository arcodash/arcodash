/** Text widgets must render Markdown, not raw text (Add Widget dialog promises "text or markdown"). */
import { afterEach, describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type * as ReactRouterModule from "react-router";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { WidgetCard } from "../WidgetCard";
import type { Widget } from "@/types/api";

const navigateSpy = vi.fn();
vi.mock("react-router", async (importOriginal) => {
  const actual = await importOriginal<typeof ReactRouterModule>();
  return { ...actual, useNavigate: () => navigateSpy };
});
vi.mock("@/lib/api", () => ({ api: {} }));
let canCreateMonitor = true;
vi.mock("@/lib/permissions", () => ({
  usePermissions: () => ({ has: (k: string) => (k === "create_monitor" ? canCreateMonitor : true) }),
}));

function textWidget(text: string): Widget {
  return {
    id: 1,
    dashboard_id: 1,
    visualization_id: null,
    text,
    width: 1,
    options: {},
    viewer_can_read_data: true,
    created_at: "2026-01-01T00:00:00.000Z",
    updated_at: "2026-01-01T00:00:00.000Z",
  };
}

function renderCard(widget: Widget, props: { publicView?: boolean; editMode?: boolean; exploreSourceId?: number | null } = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <WidgetCard widget={widget} onDelete={vi.fn()} {...props} editMode={props.editMode ?? false} />
      </QueryClientProvider>
    </MemoryRouter>,
  );
}

describe("WidgetCard text widget", () => {
  it("renders a heading and bold text as real elements, not literal markdown syntax", () => {
    renderCard(textWidget("## Solstice Commerce Overview\n\nRevenue and order trends across **Meridian Commerce**."));

    const heading = screen.getByRole("heading", { level: 2, name: "Solstice Commerce Overview" });
    expect(heading).toBeInTheDocument();

    const bold = screen.getByText("Meridian Commerce");
    expect(bold.tagName).toBe("STRONG");

    expect(screen.queryByText(/##/)).not.toBeInTheDocument();
  });

  it("does not render raw HTML tags injected in widget text (no rehype-raw)", () => {
    renderCard(textWidget('<script>window.__pwned = true</script>text'));

    expect(document.querySelector("script")).not.toBeInTheDocument();
  });
});

describe("WidgetCard monitor widget", () => {
  it("renders the monitor branch, not the text fallback, when every configured monitor is filtered out", () => {
    const widget: Widget = {
      id: 1,
      dashboard_id: 1,
      visualization_id: null,
      text: null,
      width: 1,
      options: {},
      monitors: [],
      viewer_can_read_data: true,
      created_at: "2026-01-01T00:00:00.000Z",
      updated_at: "2026-01-01T00:00:00.000Z",
    };
    renderCard(widget);

    expect(screen.getByTestId("monitor-widget")).toBeInTheDocument();
    expect(screen.getByText("Monitor status")).toBeInTheDocument();
    expect(screen.queryByText("Text")).not.toBeInTheDocument();
  });
});

function vizWidget(expectedIntervalSecs: number | null): Widget {
  return {
    ...textWidget(""),
    text: null,
    visualization_id: 5,
    freshness: {
      retrieved_at: new Date().toISOString(),
      expected_interval_secs: expectedIntervalSecs,
      last_run_status: "success",
    },
  };
}

describe("WidgetCard freshness on public views", () => {
  it("renders nothing for an unscheduled query", () => {
    renderCard(vizWidget(null), { publicView: true });

    expect(screen.queryByText(/Updated/)).not.toBeInTheDocument();
  });

  it("shows the inline age for a scheduled query", () => {
    renderCard(vizWidget(3600), { publicView: true });

    expect(screen.getByText(/Updated/)).toBeInTheDocument();
  });

  it("has no tile footer", () => {
    renderCard(vizWidget(3600), { publicView: true });

    expect(screen.queryByTestId("tile-footer")).not.toBeInTheDocument();
  });

  it("keeps the dot on private views for an unscheduled query", () => {
    renderCard(vizWidget(null));

    expect(screen.getByLabelText(/^Updated /)).toBeInTheDocument();
  });

  it("renders the age in a footer on private views", () => {
    renderCard(vizWidget(null));

    expect(screen.getByTestId("tile-footer")).toHaveTextContent("Updated");
  });
});

describe("WidgetCard truncated title tooltip", () => {
  const widths = (scrollHeight: number, clientHeight: number) => {
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockReturnValue(scrollHeight);
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(clientHeight);
  };
  afterEach(() => vi.restoreAllMocks());

  it("shows the full title when it is truncated", async () => {
    widths(200, 100);
    renderCard(textWidget("x"));

    fireEvent.focus(screen.getByText("Text"));

    expect(await screen.findByRole("tooltip")).toHaveTextContent("Text");
  });

  it("wraps the title over several lines instead of one ellipsized line", () => {
    widths(100, 100);
    renderCard(textWidget("x"));

    const el = screen.getByText("Text");
    expect(el.className).toContain("line-clamp-3");
    expect(el.className).toContain("min-w-[min(8rem,100%)]");
    expect(el.className).toContain("break-words");
    expect(el.className).not.toContain("truncate");
  });

  it("lets editor actions wrap below the title rather than squeeze it", () => {
    widths(100, 100);
    const qc = new QueryClient();
    render(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <WidgetCard widget={textWidget("x")} editMode onDelete={vi.fn()} />
        </QueryClientProvider>
      </MemoryRouter>,
    );

    const header = screen.getByTestId("widget-title-group").parentElement!;
    expect(header).toHaveClass("flex-wrap");
    expect(screen.getByLabelText("Remove Text").parentElement).toHaveClass("ml-auto");
  });

  it("shows no tooltip when the title fits", () => {
    widths(100, 100);
    renderCard(textWidget("x"));

    fireEvent.focus(screen.getByText("Text"));

    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();
  });
});

describe("WidgetCard monitor link and truncation tooltip", () => {
  afterEach(() => vi.restoreAllMocks());

  it("keeps the truncation tooltip on the linked single-monitor title", async () => {
    vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockReturnValue(200);
    vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(100);
    renderCard({
      ...textWidget(""),
      text: null,
      options: { monitors: [4] },
      monitors: [
        {
          id: 4,
          kind: "threshold",
          name: "Long name",
          state: "ok",
          column: "c",
          op: "<",
          value: 1,
          observed: 0,
          last_checked_at: null,
          muted: false,
          check_failed: false,
        },
      ],
    });

    const link = screen.getByRole("link", { name: "Long name" });
    expect(link).toHaveAttribute("href", "/monitors/4");
    fireEvent.focus(link);

    expect(await screen.findByRole("tooltip")).toHaveTextContent("Long name");
  });
});

describe("WidgetCard drag affordances", () => {
  function renderEdit(props: { editMode: boolean; draggable?: boolean }) {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return render(
      <MemoryRouter>
        <QueryClientProvider client={qc}>
          <WidgetCard widget={textWidget("x")} onDelete={vi.fn()} {...props} />
        </QueryClientProvider>
      </MemoryRouter>,
    ).container;
  }

  it("by default shows the handle, grip and select-none in edit mode", () => {
    const c = renderEdit({ editMode: true });
    const handle = c.querySelector(".tile-drag-handle");
    expect(handle).toBeInTheDocument();
    expect(handle?.querySelector("svg[aria-hidden='true']")).toBeInTheDocument();
    expect(handle?.parentElement).toHaveClass("select-none");
    expect(c.querySelector("[data-testid='widget-title-group']")?.children).toHaveLength(2);
  });

  it("has none of them in view mode", () => {
    const c = renderEdit({ editMode: false });
    expect(c.querySelector(".tile-drag-handle")).not.toBeInTheDocument();
    expect(c.querySelector(".select-none")).not.toBeInTheDocument();
  });

  it("draggable=false drops only the drag affordances; Remove stays", () => {
    const c = renderEdit({ editMode: true, draggable: false });
    expect(c.querySelector(".tile-drag-handle")).not.toBeInTheDocument();
    expect(c.querySelector("[data-testid='widget-title-group']")).not.toBeInTheDocument();
    expect(c.querySelector(".select-none")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^Remove / })).toBeInTheDocument();
  });
});

describe("WidgetCard verified badge", () => {
  const withVerified = (verified: boolean): Widget => ({
    ...vizWidget(null),
    visualization: {
      type: "TABLE",
      options: {},
      query: { id: 5, name: "Q", data_source_id: 1, parameter_names: [], options: {}, verified, is_published: true },
    },
  } as unknown as Widget);

  it("shows a shield when the query is verified", () => {
    renderCard(withVerified(true));

    expect(screen.getByRole("img", { name: "Verified by an admin" })).toBeInTheDocument();
  });

  it("shows nothing when the query is not verified", () => {
    renderCard(withVerified(false));

    expect(screen.queryByRole("img", { name: "Verified by an admin" })).not.toBeInTheDocument();
  });
});

describe("WidgetCard check badge and bell", () => {
  afterEach(() => {
    canCreateMonitor = true;
  });

  function withQuery(isPublished: boolean): Widget {
    return {
      ...vizWidget(null),
      visualization: {
        id: 5,
        name: "Viz",
        type: "table",
        options: {},
        query: { id: 42, is_published: isPublished },
      },
    } as unknown as Widget;
  }

  it("shows the bell and add-a-check link for a published query", () => {
    renderCard(withQuery(true));

    expect(screen.getByLabelText("Add a monitor for Viz")).toHaveAttribute("href", "/monitors/new?query_id=42");
    expect(screen.getByRole("link", { name: "No checks · add one" })).toBeInTheDocument();
  });

  it("shows neither for an unpublished query", () => {
    renderCard(withQuery(false));

    expect(screen.queryByLabelText("Add a monitor for Viz")).not.toBeInTheDocument();
    expect(screen.getByText("No checks")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /No checks/ })).not.toBeInTheDocument();
  });

  it("shows neither on public views", () => {
    renderCard(withQuery(true), { publicView: true });

    expect(screen.queryByLabelText("Add a monitor for Viz")).not.toBeInTheDocument();
    expect(screen.queryByText(/No checks/)).not.toBeInTheDocument();
  });

  it("shows a plain label without create_monitor", () => {
    canCreateMonitor = false;
    renderCard(withQuery(true));

    expect(screen.queryByLabelText("Add a monitor for Viz")).not.toBeInTheDocument();
    expect(screen.getByText("No checks")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /No checks/ })).not.toBeInTheDocument();
  });
});

describe("WidgetCard tile menu", () => {
  const widget = {
    ...vizWidget(null),
    viewer_can_read_data: false,
    visualization: {
      id: 5,
      name: "Viz",
      type: "table",
      options: {},
      query: { id: 42, name: "Revenue by region", is_published: true },
    },
  } as unknown as Widget;

  it("lists explore, monitor and open-query items and navigates to the explore editor", async () => {
    navigateSpy.mockClear();
    const user = userEvent.setup();
    renderCard(widget, { exploreSourceId: 9 });

    await user.click(screen.getByRole("button", { name: "Tile menu for Viz" }));
    expect(screen.getByRole("menuitem", { name: /Explore this tile/ })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Add a monitor" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Open query" })).toBeInTheDocument();

    await user.click(screen.getByRole("menuitem", { name: /Explore this tile/ }));
    expect(navigateSpy).toHaveBeenCalledWith("/queries/new", {
      state: {
        explore: { queryId: 42, queryName: "Revenue by region" },
        dataSourceId: 9,
        sql: expect.stringContaining("FROM query_42"),
      },
    });
  });

  it("omits explore when the org has no query-results source", async () => {
    const user = userEvent.setup();
    renderCard(widget, { exploreSourceId: null });

    await user.click(screen.getByRole("button", { name: "Tile menu for Viz" }));
    expect(screen.getByRole("menuitem", { name: "Open query" })).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: /Explore this tile/ })).not.toBeInTheDocument();
  });

  it("has no menu on public views or in edit mode", () => {
    renderCard(widget, { publicView: true, exploreSourceId: 9 });
    expect(screen.queryByRole("button", { name: "Tile menu for Viz" })).not.toBeInTheDocument();
  });

  it("has no menu in edit mode", () => {
    renderCard(widget, { editMode: true, exploreSourceId: 9 });
    expect(screen.queryByRole("button", { name: "Tile menu for Viz" })).not.toBeInTheDocument();
  });
});
