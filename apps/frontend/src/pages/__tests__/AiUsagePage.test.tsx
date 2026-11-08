import { describe, it, expect, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import AiUsagePage, { UsageTooltip } from "../AiUsagePage";
import { chartData } from "@/lib/usageChart";
import { setupRechartsJsdom } from "@/components/visualizations/__tests__/rechartsTestSetup";
import type * as ApiModule from "@/lib/api";
import type { AiUsageResponse, AiSettings } from "@/types/api";

setupRechartsJsdom();

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return {
    ...actual,
    api: {
      ...actual.api,
      getAiUsage: vi.fn(),
      getAiSettings: vi.fn(),
    },
  };
});

import { api } from "@/lib/api";

const mockedApi = api as unknown as {
  getAiUsage: ReturnType<typeof vi.fn>;
  getAiSettings: ReturnType<typeof vi.fn>;
};

const usage: AiUsageResponse = {
  days: 30,
  totals: { total_calls: 60000, tokens_in: 100, tokens_out: 100, success_rate: 1, cost_usd: null },
  by_provider: [],
  by_feature: [],
  by_day: [
    { date: "2026-08-01", status: "ok", count: 60000, cost_usd: null },
    { date: "2026-08-02", status: "ok", count: 30000, cost_usd: null },
  ],
  by_user: [],
  budget: { monthly_usd: null, spent_month_usd: 0, resets_at: "2026-09-01T00:00:00Z" },
};

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <AiUsagePage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("AiUsagePage chart", () => {
  it("renders a Y axis with grouped tick labels, not a bare gridline chart", async () => {
    mockedApi.getAiUsage.mockResolvedValue(usage);
    mockedApi.getAiSettings.mockResolvedValue({ provider_order: [] } as unknown as AiSettings);

    const { container } = renderPage();
    await screen.findByText("Calls per day");

    const yTicks = container.querySelectorAll(
      ".recharts-yAxis-tick-labels .recharts-cartesian-axis-tick-value",
    );
    expect(yTicks.length).toBeGreaterThan(0);
    // A raw "60000" tick would mean the axis fell back to recharts' default formatting.
    expect(Array.from(yTicks).some((t) => /,/.test(t.textContent ?? ""))).toBe(true);
  });

  it("keeps the title fixed above the tab strip — navigating Providers -> Usage must not relocate it", async () => {
    mockedApi.getAiUsage.mockResolvedValue(usage);
    mockedApi.getAiSettings.mockResolvedValue({ provider_order: [] } as unknown as AiSettings);

    renderPage();
    const heading = await screen.findByRole("heading", { name: "AI Usage" });
    const tablist = await screen.findByRole("tablist");

    // DOCUMENT_POSITION_FOLLOWING means `tablist` comes after `heading` in the DOM.
    expect(heading.compareDocumentPosition(tablist) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("renders labelled by-feature rows and falls back to the raw key for unknown features", async () => {
    mockedApi.getAiUsage.mockResolvedValue({
      ...usage,
      by_feature: [
        { feature: "explain_alert_plan", calls: 4, tokens_in: 10, tokens_out: 5, error_rate: 0.25 },
        { feature: "brand_new_feature", calls: 2, tokens_in: 1, tokens_out: 1, error_rate: 0 },
      ],
    });
    mockedApi.getAiSettings.mockResolvedValue({ provider_order: [] } as unknown as AiSettings);

    renderPage();
    expect(await screen.findByText("Alert explanation (plan)")).toBeInTheDocument();
    expect(screen.getByText("brand_new_feature")).toBeInTheDocument();
    expect(screen.getByText("25.0%")).toBeInTheDocument();
  });
});

const FOOTNOTE =
  "Cost uses list prices; unknown models are not priced. Unpriced calls show — and don't count toward the monthly budget, which resets on the 1st of each month (UTC).";

async function renderWith(over: Partial<AiUsageResponse>) {
  mockedApi.getAiUsage.mockResolvedValue({ ...usage, ...over });
  mockedApi.getAiSettings.mockResolvedValue({ provider_order: [] } as unknown as AiSettings);
  renderPage();
  return screen.findByTestId("ai-budget-summary");
}

const budget = (monthly_usd: number | null, spent_month_usd: number) => ({
  monthly_usd,
  spent_month_usd,
  resets_at: "2026-11-01T00:00:00Z",
});

describe("AiUsagePage budget summary", () => {
  it.each([
    [10, 4, 40, "bg-foreground/60"],
    [10, 8.5, 85, "bg-warning"],
    [10, 12, 100, "bg-destructive"],
  ])("cap %s spent %s: fill class and aria values", async (cap, spent, now, cls) => {
    const summary = await renderWith({ budget: budget(cap, spent) });
    const bar = screen.getByRole("progressbar", { name: "Monthly AI budget used" });
    expect(bar).toHaveAttribute("aria-valuemin", "0");
    expect(bar).toHaveAttribute("aria-valuemax", "100");
    expect(bar).toHaveAttribute("aria-valuenow", String(now));
    expect(bar.firstElementChild).toHaveClass(cls);
    expect(within(summary).getByText(/resets Nov 1 \(UTC\)/)).toBeInTheDocument();
  });

  it("over cap shows the paused sentence and the real percentage", async () => {
    const summary = await renderWith({ budget: budget(10, 12) });
    expect(screen.getByTestId("ai-budget-paused")).toHaveTextContent(
      "AI features are paused until Nov 1. Alerts still send without AI analysis.",
    );
    expect(screen.getByTestId("ai-budget-bar")).toHaveAttribute("aria-valuetext", "$12.00 of $10.00 (120%)");
    expect(within(summary).getByText(/120%/)).toHaveClass("text-destructive");
  });

  it("99.6% of cap rounds to 100% but is neither paused nor destructive", async () => {
    const summary = await renderWith({ budget: budget(10, 9.96) });
    expect(screen.queryByTestId("ai-budget-paused")).not.toBeInTheDocument();
    expect(within(summary).getByText(/100%/)).not.toHaveClass("text-destructive");
    expect(screen.getByTestId("ai-budget-bar").firstElementChild).not.toHaveClass("bg-destructive");
  });

  it("exactly at cap is paused", async () => {
    const summary = await renderWith({ budget: budget(10, 10) });
    expect(screen.getByTestId("ai-budget-paused")).toBeInTheDocument();
    expect(within(summary).getByText(/100%/)).toHaveClass("text-destructive");
  });

  it("under cap shows no paused sentence", async () => {
    await renderWith({ budget: budget(10, 8.5) });
    expect(screen.queryByTestId("ai-budget-paused")).not.toBeInTheDocument();
  });

  it("no cap: text, Set a budget link, no bar", async () => {
    const summary = await renderWith({ budget: budget(null, 3) });
    expect(summary).toHaveTextContent("Spent this month $3.00, no cap");
    expect(screen.getByRole("link", { name: "Set a budget" })).toHaveAttribute("href", "/organization/ai");
    expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
  });

  it("still renders when there is no usage", async () => {
    await renderWith({
      totals: { ...usage.totals, total_calls: 0 },
      budget: budget(10, 2),
    });
    expect(screen.getByText(/No AI usage yet/)).toBeInTheDocument();
    expect(screen.getByRole("progressbar")).toBeInTheDocument();
    expect(screen.queryByTestId("ai-cost-footnote")).not.toBeInTheDocument();
  });
});

describe("AiUsagePage cost and by-user", () => {
  it("renders the Cost tile and cost cells, null as an em dash", async () => {
    mockedApi.getAiUsage.mockResolvedValue({
      ...usage,
      totals: { ...usage.totals, cost_usd: 12.4 },
      by_provider: [
        { provider: "gemini", calls: 1, tokens_in: 1, tokens_out: 1, error_rate: 0, cost_usd: 0.004 },
        { provider: "openai", calls: 1, tokens_in: 1, tokens_out: 1, error_rate: 0, cost_usd: null },
      ],
      by_feature: [{ feature: "ask", calls: 1, tokens_in: 1, tokens_out: 1, error_rate: 0, cost_usd: 1234.5 }],
    });
    mockedApi.getAiSettings.mockResolvedValue({ provider_order: [] } as unknown as AiSettings);
    renderPage();
    expect(await screen.findByText("$12.40")).toBeInTheDocument();
    expect(screen.getByText("Cost", { selector: "div" })).toBeInTheDocument();
    expect(screen.getByText("<$0.01")).toBeInTheDocument();
    expect(screen.getByText("—")).toBeInTheDocument();
    expect(screen.getByText("$1,234.50")).toBeInTheDocument();
  });

  it("labels by-user rows in server order and shows the footnote", async () => {
    mockedApi.getAiUsage.mockResolvedValue({
      ...usage,
      by_user: [
        { user_id: null, email: null, name: null, calls: 9, tokens_in: 1, tokens_out: 1, cost_usd: 5 },
        { user_id: 7, email: null, name: null, calls: 8, tokens_in: 1, tokens_out: 1, cost_usd: 4 },
        { user_id: 1, email: "ada@x.io", name: "Ada", calls: 7, tokens_in: 1, tokens_out: 1, cost_usd: 3 },
        { user_id: 2, email: "bob@x.io", name: null, calls: 6, tokens_in: 1, tokens_out: 1, cost_usd: null },
      ],
    });
    mockedApi.getAiSettings.mockResolvedValue({ provider_order: [] } as unknown as AiSettings);
    renderPage();
    const card = await screen.findByTestId("ai-usage-by-user");
    const rows = within(card).getAllByRole("row").slice(1);
    expect(rows[0]).toHaveTextContent("System (background jobs)");
    expect(rows[1]).toHaveTextContent("Deleted user");
    expect(rows[2]).toHaveTextContent("Ada");
    expect(within(rows[2]).getByText("ada@x.io")).toHaveClass("text-xs");
    expect(rows[3]).toHaveTextContent("bob@x.io");
    expect(rows[3]).toHaveTextContent("—");
    expect(screen.getByTestId("ai-cost-footnote")).toHaveTextContent(FOOTNOTE);
  });
});

describe("chartData cost", () => {
  it("sums cost per date across statuses, null only when every row is null", () => {
    const rows = chartData([
      { date: "2026-08-01", status: "ok", count: 1, cost_usd: 1.5 },
      { date: "2026-08-01", status: "error", count: 1, cost_usd: null },
      { date: "2026-08-01", status: "quota_exceeded", count: 1, cost_usd: 0.5 },
      { date: "2026-08-02", status: "ok", count: 1, cost_usd: null },
    ]);
    expect(rows.map((r) => r.cost)).toEqual([2, null]);
  });
});

describe("UsageTooltip cost line", () => {
  const item = (cost: number | null) => [{ dataKey: "ok" as const, value: 3, payload: { cost } }];

  it("shows the cost when priced", () => {
    render(<UsageTooltip active label="2026-10-01" payload={item(1.5)} />);
    expect(screen.getByText("$1.50")).toBeInTheDocument();
    expect(screen.getByText("cost")).toBeInTheDocument();
  });

  it("shows a dash when unpriced", () => {
    render(<UsageTooltip active label="2026-10-01" payload={item(null)} />);
    expect(screen.getByText("—")).toBeInTheDocument();
  });
});
