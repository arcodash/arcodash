/** Text widgets must render Markdown, not raw text (Add Widget dialog promises "text or markdown"). */
import { afterEach, describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { extensions } from "@/extensions";
import { WidgetCard } from "../WidgetCard";
import type { Widget } from "@/types/api";

vi.mock("@/lib/api", () => ({ api: {} }));

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

function renderCard(widget: Widget, props: { publicView?: boolean } = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <WidgetCard widget={widget} editMode={false} onDelete={vi.fn()} {...props} />
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

  it("keeps the dot on private views for an unscheduled query", () => {
    renderCard(vizWidget(null));

    expect(screen.getByLabelText(/^Updated /)).toBeInTheDocument();
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

describe("WidgetCard result footnote slot", () => {
  afterEach(() => {
    extensions.length = 0;
  });

  it("renders_result_footnote_slot_when_provided", () => {
    const widget = vizWidget(3600);
    extensions.push({
      navItems: [],
      routes: [],
      resultFootnoteSlot: (p) => (
        <span data-testid="ext-footnote">
          {p.placement}|{p.retrievedAt}|{String(p.publicView)}
        </span>
      ),
    });

    renderCard(widget, { publicView: true });

    expect(screen.getByTestId("ext-footnote")).toHaveTextContent(
      `widget|${widget.freshness!.retrieved_at}|true`,
    );
    expect(screen.getByTestId("widget-result-footnote")).toContainElement(screen.getByTestId("ext-footnote"));
  });

  it("renders_nothing_without_extension", () => {
    renderCard(vizWidget(3600));

    expect(screen.queryByTestId("widget-result-footnote")).not.toBeInTheDocument();
  });
});
