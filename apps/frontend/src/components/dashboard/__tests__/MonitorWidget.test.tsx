import { describe, expect, it } from "vitest";
import { render as rtlRender, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import type { ReactElement } from "react";
import { MonitorWidget } from "../MonitorWidget";
import { WidgetCard } from "../WidgetCard";
import type { MonitorTile, Widget } from "@/types/api";

const render = (ui: ReactElement) => rtlRender(<MemoryRouter>{ui}</MemoryRouter>);

function tile(overrides: Partial<MonitorTile> = {}): MonitorTile {
  return {
    id: 1,
    name: "Order backlog",
    state: "ok",
    column: "backlog",
    op: "<",
    value: 100,
    observed: 42,
    last_checked_at: "2026-01-01T00:00:00.000Z",
    muted: false,
    kind: "threshold",
    check_failed: false,
    ...overrides,
  };
}

function monitorWidget(ids: number[], tiles: MonitorTile[]): Widget {
  return {
    id: 1,
    dashboard_id: 1,
    visualization_id: null,
    viewer_can_read_data: true,
    text: null,
    width: 1,
    options: { monitors: ids },
    monitors: tiles,
    created_at: "2026-01-01T00:00:00.000Z",
    updated_at: "2026-01-01T00:00:00.000Z",
  };
}

describe("MonitorWidget single tile", () => {
  it("renders Alerting, the observed number, and red root chrome for a triggered monitor", () => {
    const w = monitorWidget([1], [tile({ id: 1, state: "triggered", observed: 913 })]);
    render(<WidgetCard widget={w} editMode={false} onDelete={() => {}} />);

    expect(screen.getByText("Alerting")).toBeInTheDocument();
    expect(screen.getByText("913")).toBeInTheDocument();
    const root = screen.getByTestId("monitor-widget").closest("div.flex.h-full.flex-col.rounded-md");
    expect(root?.className).toContain("border-destructive");
  });

  it("sizes the observed value to its container and never clips it", () => {
    render(<MonitorWidget widget={monitorWidget([1], [tile({ id: 1, observed: 1598234 })])} />);

    const value = screen.getByText("1,598,234");
    expect(value.className).not.toContain("truncate");
    expect(value).toHaveAttribute("title", "1,598,234");
    expect(value.style.fontSize).toContain("cqw");
  });

  it("renders a dash for a null observed value without throwing", () => {
    render(<MonitorWidget widget={monitorWidget([1], [tile({ id: 1, observed: null })])} />);

    expect(screen.getByText("—")).toBeInTheDocument();
  });

  it("stringifies a non-numeric observed value instead of throwing", () => {
    render(<MonitorWidget widget={monitorWidget([1], [tile({ id: 1, observed: true })])} />);

    expect(screen.getByText("true")).toBeInTheDocument();
  });

  it("shows the unavailable copy and no badge when the configured id has no matching tile", () => {
    render(<MonitorWidget widget={monitorWidget([99], [])} />);

    expect(
      screen.getByText("This monitor was deleted or isn't shared with you."),
    ).toBeInTheDocument();
    expect(screen.queryByTestId("monitor-widget-state")).not.toBeInTheDocument();
  });
});

describe("MonitorWidget list", () => {
  it("renders 3 rows sorted by severity regardless of insertion order", () => {
    const tiles = [
      tile({ id: 2, name: "Zebra", state: "ok" }),
      tile({ id: 3, name: "Apple", state: "triggered" }),
      tile({ id: 1, name: "Middle", state: "unevaluable" }),
    ];
    render(<MonitorWidget widget={monitorWidget([2, 3, 1], tiles)} />);

    const rows = screen.getAllByTestId("monitor-widget-row");
    expect(rows).toHaveLength(3);
    expect(rows.map((r) => r.textContent)).toEqual([
      expect.stringContaining("Apple"),
      expect.stringContaining("Middle"),
      expect.stringContaining("Zebra"),
    ]);
  });

  it("renders an Unavailable row for a configured id missing from the tiles", () => {
    const tiles = [tile({ id: 1, name: "Present", state: "ok" })];
    render(<MonitorWidget widget={monitorWidget([1, 2], tiles)} />);

    const rows = screen.getAllByTestId("monitor-widget-row");
    expect(rows).toHaveLength(2);
    expect(screen.getByText("Unavailable monitor")).toBeInTheDocument();
    expect(screen.getByText("Unavailable")).toBeInTheDocument();
  });

  it("starts the row's native title with the monitor name", () => {
    render(<MonitorWidget widget={monitorWidget([1, 2], [tile({ id: 1, name: "Alpha" }), tile({ id: 2, name: "Beta" })])} />);

    const row = screen.getAllByTestId("monitor-widget-row")[0];
    expect(row.getAttribute("title")).toMatch(/^(Alpha|Beta) · /);
  });
});

describe("monitor card links", () => {
  const card = (w: Widget, props: { editMode?: boolean; publicView?: boolean } = {}) =>
    render(
      <WidgetCard
        widget={w}
        editMode={props.editMode ?? false}
        publicView={props.publicView}
        onDelete={() => {}}
      />,
    );

  it("single card links to the monitor, named by the monitor name", () => {
    card(monitorWidget([7], [tile({ id: 7, name: "Backlog" })]));

    const link = screen.getByRole("link", { name: "Backlog" });
    expect(link).toHaveAttribute("href", "/monitors/7");
    expect(screen.getAllByRole("link")).toHaveLength(1);
  });

  it("list links each available row and not the unavailable one", () => {
    card(monitorWidget([1, 2, 3], [tile({ id: 1, name: "Alpha" }), tile({ id: 2, name: "Beta" })]));

    expect(screen.getByRole("link", { name: "Alpha" })).toHaveAttribute("href", "/monitors/1");
    expect(screen.getByRole("link", { name: "Beta" })).toHaveAttribute("href", "/monitors/2");
    expect(screen.getAllByRole("link")).toHaveLength(2);
    expect(screen.queryByRole("link", { name: "Monitor status" })).not.toBeInTheDocument();
  });

  it("renders no links in edit mode", () => {
    card(monitorWidget([1, 2], [tile({ id: 1 }), tile({ id: 2, name: "B" })]), { editMode: true });
    card(monitorWidget([7], [tile({ id: 7 })]), { editMode: true });

    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });

  it("renders no links in public view", () => {
    card(monitorWidget([1, 2], [tile({ id: 1 }), tile({ id: 2, name: "B" })]), { publicView: true });
    card(monitorWidget([7], [tile({ id: 7 })]), { publicView: true });

    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });

  it("renders no link for a deleted or unshared single monitor", () => {
    card(monitorWidget([99], []));

    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });

  it("MonitorWidget list rows are plain text unless linkable", () => {
    render(<MonitorWidget widget={monitorWidget([1, 2], [tile({ id: 1 }), tile({ id: 2, name: "B" })])} />);

    expect(screen.queryAllByRole("link")).toHaveLength(0);
  });
});

describe("MonitorWidget baseline", () => {
  const baseline = { kind: "baseline" as const, op: null, value: null, learning: { have: 3, need: 7 } };

  it("shows the learning badge and 'vs. recent pattern' instead of a comparison", () => {
    render(<MonitorWidget widget={monitorWidget([1], [tile(baseline)])} />);

    expect(screen.getByTestId("monitor-learning-badge")).toHaveTextContent("Learning (3/7)");
    expect(screen.getByText(/vs\. recent pattern/)).toBeInTheDocument();
    expect(screen.queryByText(/backlog\s*>/)).not.toBeInTheDocument();
  });

  it("omits the badge once learning is done", () => {
    render(<MonitorWidget widget={monitorWidget([1], [tile({ ...baseline, learning: null })])} />);

    expect(screen.queryByTestId("monitor-learning-badge")).not.toBeInTheDocument();
  });

  it("list rows carry the baseline condition and learning progress in the title", () => {
    render(<MonitorWidget widget={monitorWidget([1, 2], [tile({ id: 1, ...baseline }), tile({ id: 2 })])} />);

    const rows = screen.getAllByTestId("monitor-widget-row");
    expect(rows[0]).toHaveAttribute("title", expect.stringContaining("backlog vs. recent pattern · "));
    expect(rows[0]).toHaveAttribute("title", expect.stringContaining("Learning 3/7"));
    expect(screen.getByText("Learning (3/7)", { exact: false })).toBeInTheDocument();
  });
});

describe("MonitorWidget incident summary", () => {
  it("shows incident_summary as a focusable button on a triggered tile only", () => {
    const summary = "backlog is 913, above 100";
    const { rerender } = render(
      <MonitorWidget widget={monitorWidget([1], [tile({ state: "triggered", incident_summary: summary })])} />,
    );
    const btn = screen.getByTestId("monitor-widget-incident");
    expect(btn.tagName).toBe("BUTTON");
    expect(btn).toHaveTextContent(summary);

    rerender(
      <MemoryRouter>
        <MonitorWidget widget={monitorWidget([1], [tile({ state: "ok", incident_summary: summary })])} />
      </MemoryRouter>,
    );
    expect(screen.queryByTestId("monitor-widget-incident")).not.toBeInTheDocument();
  });
});

describe("MonitorWidget failed check", () => {
  it("shows a tooltip trigger with the error when one is present", () => {
    const w = monitorWidget([1], [tile({ state: "ok", check_failed: true, last_check_error: "relation does not exist" })]);
    render(<MonitorWidget widget={w} />);

    expect(screen.getByText("Check failed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check failed: relation does not exist" })).toBeInTheDocument();
    expect(screen.getByText(/Last check failed/)).toHaveClass("text-destructive");
    expect(screen.queryByText(/^Checked /)).not.toBeInTheDocument();
    expect(screen.getByTestId("monitor-widget-value")).toHaveClass("text-muted-foreground");
  });

  it("has no button and a title when the error is hidden (public view)", () => {
    const w = monitorWidget([1], [tile({ state: "unknown", check_failed: true, last_check_error: null, observed: null })]);
    render(<MonitorWidget widget={w} />);

    expect(screen.queryByRole("button")).not.toBeInTheDocument();
    expect(screen.getByText("Check failed").closest("[title]")).toHaveAttribute("title", "The last check didn't finish.");
    expect(screen.getByText(/Last check failed/)).toBeInTheDocument();
  });

  it("keeps Alerting and moves the error tooltip to the footer when triggered", () => {
    const w = monitorWidget([1], [tile({ state: "triggered", check_failed: true, last_check_error: "boom" })]);
    render(<MonitorWidget widget={w} />);

    expect(screen.getByText("Alerting")).toBeInTheDocument();
    expect(screen.queryByText("Check failed")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check failed: boom" })).toHaveTextContent(/Last check failed/);
  });

  it("omits the unevaluable line while the check failed", () => {
    const w = monitorWidget([1], [tile({ state: "unevaluable", check_failed: true })]);
    render(<MonitorWidget widget={w} />);

    expect(screen.queryByText(/couldn't compare/)).not.toBeInTheDocument();
  });

  it("ranks rows triggered > check failed > unevaluable > unknown > ok and excludes failed from OK", () => {
    const tiles = [
      tile({ id: 1, name: "a-ok", state: "ok" }),
      tile({ id: 2, name: "b-unknown", state: "unknown" }),
      tile({ id: 3, name: "c-unev", state: "unevaluable" }),
      tile({ id: 4, name: "d-failed", state: "ok", check_failed: true }),
      tile({ id: 5, name: "e-trig", state: "triggered", check_failed: true }),
    ];
    render(<MonitorWidget widget={monitorWidget([1, 2, 3, 4, 5], tiles)} />);

    const rows = screen.getAllByTestId("monitor-widget-row");
    expect(rows.map((r) => r.getAttribute("title")?.split(" · ")[0])).toEqual([
      "e-trig", "d-failed", "c-unev", "b-unknown", "a-ok",
    ]);
    expect(rows[1].className).toContain("border-l-destructive");
    expect(rows[1].className).not.toContain("bg-destructive/10");
    expect(rows[1].getAttribute("title")).toContain("Last check failed");
    expect(screen.getByText("1 check failed")).toBeInTheDocument();
    expect(screen.getByText("1 check failed").parentElement).toHaveTextContent("1 alerting · 1 check failed · 1 OK");
  });
});
