import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import AiSettingsPage from "../AiSettingsPage";
import type * as ApiModule from "@/lib/api";
import type { AiSettings } from "@/types/api";

vi.mock("@/lib/auth", () => ({ useAuth: () => ({ refreshSession: vi.fn() }) }));
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  return { ...actual, api: { ...actual.api, getAiSettings: vi.fn(), updateAiSettings: vi.fn() } };
});

import { api } from "@/lib/api";

const mocked = api as unknown as {
  getAiSettings: ReturnType<typeof vi.fn>;
  updateAiSettings: ReturnType<typeof vi.fn>;
};

const settings = (over: Partial<AiSettings> = {}) =>
  ({
    key_configured: true,
    key_source: "org",
    rate_limit_org_value: null,
    rate_limit_env_default: 10,
    rate_limit_effective: 10,
    provider_order: [],
    fallback_enabled: false,
    include_sample_values: true,
    explain_alerts: true,
    monthly_budget_usd: null,
    providers: Object.fromEntries(
      ["gemini", "openai", "anthropic"].map((k) => [
        k,
        { key_configured: false, key_source: "none", model: null, base_url: null, default_model: "m", curated_models: ["m"], last_status: null, last_status_at: null },
      ]),
    ),
    ...over,
  }) as AiSettings;

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <AiSettingsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("AiSettingsPage explain_alerts", () => {
  beforeEach(() => vi.clearAllMocks());

  it("shows the disclosure and sends explain_alerts: false on toggle", async () => {
    mocked.getAiSettings.mockResolvedValue(settings());
    mocked.updateAiSettings.mockResolvedValue(settings({ explain_alerts: false }));
    renderPage();
    const sw = await screen.findByTestId("ai-explain-alerts-switch");
    expect(sw).toHaveAttribute("aria-describedby", "ai-explain-alerts-help");
    expect(screen.getByText(/aggregated rows are sent to your AI provider/)).toBeInTheDocument();
    await userEvent.click(sw);
    expect(mocked.updateAiSettings).toHaveBeenCalledWith({ explain_alerts: false });
  });

  it("is disabled with a hint when sample values are off", async () => {
    mocked.getAiSettings.mockResolvedValue(settings({ include_sample_values: false }));
    renderPage();
    const sw = await screen.findByTestId("ai-explain-alerts-switch");
    expect(sw).toBeDisabled();
    expect(sw).not.toBeChecked();
    expect(screen.getAllByText(/Requires .Include sample values./).length).toBeGreaterThan(0);
  });
});

describe("AiSettingsPage dashboard_narratives", () => {
  beforeEach(() => vi.clearAllMocks());

  it("sends dashboard_narratives: false on toggle", async () => {
    mocked.getAiSettings.mockResolvedValue(settings({ dashboard_narratives: true }));
    mocked.updateAiSettings.mockResolvedValue(settings({ dashboard_narratives: false }));
    renderPage();
    await userEvent.click(await screen.findByTestId("ai-dashboard-narratives-switch"));
    expect(mocked.updateAiSettings).toHaveBeenCalledWith({ dashboard_narratives: false });
  });

  it("is disabled when sample values are off", async () => {
    mocked.getAiSettings.mockResolvedValue(settings({ include_sample_values: false }));
    renderPage();
    expect(await screen.findByTestId("ai-dashboard-narratives-switch")).toBeDisabled();
  });
});
