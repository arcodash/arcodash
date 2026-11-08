import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { DashboardNarrativePanel } from "../DashboardNarrativePanel";
import type { DashboardNarrative, Widget } from "@/types/api";

const widget = (id: number, name: string) =>
  ({ id, visualization_id: id, visualization: { name }, options: {}, text: null }) as unknown as Widget;

const narrative = (over: Partial<DashboardNarrative> = {}): DashboardNarrative => ({
  id: 1,
  dashboard_id: 1,
  created_at: new Date().toISOString(),
  source: "manual",
  summary: "Revenue is up.",
  body: null,
  highlights: [{ widget_id: 10, text: "Revenue rose 5%" }],
  confidence: "high",
  model: "gpt-x",
  ...over,
});

function renderPanel(props: Partial<React.ComponentProps<typeof DashboardNarrativePanel>> = {}) {
  const onRegenerate = vi.fn();
  const onToggle = vi.fn();
  render(
    <MemoryRouter>
      <DashboardNarrativePanel
        widgets={[widget(10, "Revenue")]}
        narrative={narrative()}
        expanded
        onToggle={onToggle}
        pending={false}
        error={null}
        onRegenerate={onRegenerate}
        isAdmin={false}
        {...props}
      />
    </MemoryRouter>,
  );
  return { onRegenerate, onToggle };
}

describe("DashboardNarrativePanel", () => {
  it("renders summary, confidence, model disclosure", () => {
    renderPanel();
    expect(screen.getByText("Revenue is up.")).toBeInTheDocument();
    expect(screen.getByText("High confidence")).toBeInTheDocument();
    expect(screen.getByText(/Written by gpt-x/)).toBeInTheDocument();
  });

  it("chip click scrolls to, focuses and outlines the widget card", async () => {
    const card = document.createElement("div");
    card.setAttribute("data-widget-id", "10");
    card.tabIndex = -1;
    card.scrollIntoView = vi.fn();
    document.body.appendChild(card);
    renderPanel();
    await userEvent.click(screen.getByRole("button", { name: "Show widget Revenue: Revenue rose 5%" }));
    expect(card.scrollIntoView).toHaveBeenCalled();
    expect(document.activeElement).toBe(card);
    expect(card.classList.contains("ring-primary")).toBe(true);
    card.remove();
  });

  it("renders a highlight for a widget not on the grid as plain text", () => {
    renderPanel({ widgets: [] });
    expect(screen.getByText("Revenue rose 5%")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Show widget/ })).toBeNull();
  });

  it("renders a markdown body instead of summary and highlights", () => {
    renderPanel({ narrative: narrative({ body: "Orders are **up**.\n\n- one\n- two" }) });
    expect(screen.getByText("up").tagName).toBe("STRONG");
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
    expect(screen.queryByText("Revenue is up.")).toBeNull();
    expect(screen.queryByText("Revenue rose 5%")).toBeNull();
  });

  it("renders an escaped bracket literally with no link", () => {
    renderPanel({ narrative: narrative({ body: "see \\[x\\]: https://evil.com" }) });
    expect(screen.getByText("see [x]: https://evil.com")).toBeInTheDocument();
    expect(screen.queryByRole("link")).toBeNull();
    expect(screen.queryByRole("button", { name: "x" })).toBeNull();
  });

  it("widget link in the body scrolls to and focuses the widget", async () => {
    const card = document.createElement("div");
    card.setAttribute("data-widget-id", "10");
    card.tabIndex = -1;
    card.scrollIntoView = vi.fn();
    document.body.appendChild(card);
    renderPanel({ narrative: narrative({ body: "See [Revenue](#widget-10) now." }) });
    const link = screen.getByRole("button", { name: "Revenue" });
    expect(link).toHaveAttribute("title", "Show widget Revenue");
    await userEvent.click(link);
    expect(card.scrollIntoView).toHaveBeenCalled();
    expect(document.activeElement).toBe(card);
    card.remove();
  });

  it("widget link to a missing widget renders as plain text", () => {
    renderPanel({ narrative: narrative({ body: "See [Gone](#widget-99)." }) });
    expect(screen.getByText(/See Gone\./)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Gone" })).toBeNull();
  });

  it("drops foreign links, raw HTML, scripts and images from the body", () => {
    const { container } = render(
      <MemoryRouter>
        <DashboardNarrativePanel
          widgets={[widget(10, "Revenue")]}
          narrative={narrative({
            body: "[evil](https://evil.test) <script>alert(1)</script><b>raw</b> ![img](https://evil.test/x.png) text",
          })}
          expanded
          onToggle={vi.fn()}
          pending={false}
          error={null}
          onRegenerate={vi.fn()}
          isAdmin={false}
        />
      </MemoryRouter>,
    );
    const body = container.querySelector(".max-w-prose")!;
    expect(body.querySelector("a, script, b, img")).toBeNull();
    expect(screen.queryByRole("link", { name: "evil" })).toBeNull();
    expect(body.textContent).toContain("evil");
  });

  it("collapsed shows the summary even when a body exists", () => {
    renderPanel({ expanded: false, narrative: narrative({ body: "Long **body**." }) });
    expect(screen.getByText("Revenue is up.")).toBeInTheDocument();
    expect(screen.queryByText("body")).toBeNull();
  });

  it("collapsed shows a toggle with aria-expanded=false", async () => {
    const { onToggle } = renderPanel({ expanded: false });
    const btn = screen.getByRole("button", { name: /AI summary/ });
    expect(btn).toHaveAttribute("aria-expanded", "false");
    expect(btn).not.toHaveAttribute("aria-controls");
    await userEvent.click(btn);
    expect(onToggle).toHaveBeenCalled();
  });

  it("low confidence and digest source add their lines", () => {
    renderPanel({ narrative: narrative({ confidence: "low", source: "digest" }) });
    expect(screen.getByText(/Low confidence — check the widgets/)).toBeInTheDocument();
    expect(screen.getByText(/From your morning brief/)).toBeInTheDocument();
  });

  it("flags a summary older than 24h", () => {
    renderPanel({ narrative: narrative({ created_at: new Date(Date.now() - 30 * 3600_000).toISOString() }) });
    expect(screen.getByText(/Widgets may have changed/)).toBeInTheDocument();
  });

  it("first run shows busy skeleton and widget count", () => {
    renderPanel({ narrative: null, pending: true });
    expect(screen.getByText("Reading 1 widget…")).toBeInTheDocument();
  });

  it("regenerating keeps the old summary", () => {
    renderPanel({ pending: true });
    expect(screen.getByText("Revenue is up.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Regenerating/ })).toBeDisabled();
  });

  it("generation_failed shows message and Try again; disabled shows admin link only for admins", async () => {
    const { onRegenerate } = renderPanel({
      narrative: null,
      error: { status: "generation_failed", message: "Boom." },
    });
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(onRegenerate).toHaveBeenCalled();
  });

  it("disabled error links admins to AI settings", () => {
    renderPanel({ narrative: null, isAdmin: true, error: { status: "disabled", message: "Off." } });
    expect(screen.getByRole("link", { name: "Turn on in AI Settings" })).toHaveAttribute("href", "/organization/ai");
  });
});
