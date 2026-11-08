import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, act } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";
import AiSettingsPage from "@/pages/AiSettingsPage";
import type { AiProviderInfo, AiSettings } from "@/types/api";

const BASE = "http://localhost:8080";

function providerInfo(defaultModel: string, curatedModels: string[], overrides: Partial<AiProviderInfo> = {}): AiProviderInfo {
  return {
    key_configured: false,
    key_source: "none",
    model: defaultModel,
    base_url: null,
    default_model: defaultModel,
    curated_models: curatedModels,
    last_status: null,
    last_status_at: null,
    ...overrides,
  };
}

const BASE_SETTINGS: AiSettings = {
  key_configured: false,
  key_source: "none",
  rate_limit_org_value: null,
  rate_limit_env_default: 10,
  rate_limit_effective: 10,
  provider_order: [],
  fallback_enabled: false,
  include_sample_values: true,
  monthly_budget_usd: null,
  providers: {
    gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite", "gemini-3.6-flash", "gemini-3.1-pro-preview"]),
    openai: providerInfo("gpt-5.6-terra", ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"]),
    anthropic: providerInfo("claude-sonnet-5", ["claude-sonnet-5", "claude-opus-5", "claude-haiku-4-5"]),
  },
};

function mockAiSettings(settings: AiSettings) {
  server.use(http.get(`${BASE}/api/organization/ai`, () => HttpResponse.json(settings)));
}

/** Counts GET /api/session calls; the session query only refetches when `refreshSession()` runs. */
function trackSessionRefetches() {
  let calls = 0;
  server.use(
    http.get(`${BASE}/api/session`, () => {
      calls += 1;
      return HttpResponse.json({
        user: { id: 1, name: "Test User", email: "test@example.com", groups: [1], org_id: 1 },
        org: { id: 1, name: "Test Org", slug: "test-org" },
        client_config: {
          disable_public_urls: false,
          saml_enabled: false,
          ldap_enabled: false,
          ai_sql_generation: false,
        },
      });
    }),
  );
  return () => calls;
}

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <AuthProvider>
          <AiSettingsPage />
        </AuthProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("AiSettingsPage", () => {
  it("renders the page heading above the Providers/Usage tab bar, not beneath it, and sets the document title", async () => {
    mockAiSettings(BASE_SETTINGS);
    renderPage();

    const heading = await screen.findByRole("heading", { name: "AI Settings" });
    const tab = screen.getByRole("tab", { name: "Providers" });
    expect(heading.compareDocumentPosition(tab) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(document.title).toBe("Providers · arcodash");
  });

  it("shows a neutral Busy badge, not Failed, for an unavailable provider", async () => {
    mockAiSettings({
      ...BASE_SETTINGS,
      providers: {
        ...BASE_SETTINGS.providers,
        gemini: providerInfo("gemini-3.1-flash-lite", [], { last_status: "unavailable" }),
      },
    });
    renderPage();

    const badge = await screen.findByText("Busy");
    expect(badge).toHaveAttribute("title", expect.stringContaining("high demand"));
    expect(screen.queryByText("Failed")).not.toBeInTheDocument();
  });

  it("names all AI features in the usage-limit copy, not just SQL generation", async () => {
    mockAiSettings(BASE_SETTINGS);

    renderPage();
    await screen.findByText("Usage limit");

    expect(screen.getByLabelText("AI feature calls per user, per minute")).toBeInTheDocument();
    expect(screen.getByText(/limits how often each user can call ai features/i)).toBeInTheDocument();
    expect(screen.queryByText(/generate sql/i)).not.toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: /learn more about ai usage limits/i }),
    ).toHaveAttribute("href", "https://www.arcodash.com/docs/reference/limits#ai-usage");
  });

  it("Save is disabled with a reason until something on the card actually changes", async () => {
    mockAiSettings(BASE_SETTINGS);

    renderPage();
    const geminiKeyInput = await screen.findByLabelText("API key", { selector: "#gemini-key" });
    const saveButton = screen.getAllByRole("button", { name: /^save$/i })[0];

    expect(saveButton).toBeDisabled();
    expect(saveButton).toHaveAttribute("title", "Enter an API key or change the model/base URL first");

    await userEvent.type(geminiKeyInput, "a".repeat(24));
    expect(saveButton).toBeEnabled();
    expect(saveButton).not.toHaveAttribute("title");
  });

  it("refreshes the session after a successful key save", async () => {
    mockAiSettings(BASE_SETTINGS);
    const getSessionCalls = trackSessionRefetches();
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        HttpResponse.json({
          ...BASE_SETTINGS,
          providers: { ...BASE_SETTINGS.providers, gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }) },
          provider_order: ["gemini"],
        }),
      ),
    );

    renderPage();
    const geminiKeyInput = await screen.findByLabelText("API key", { selector: "#gemini-key" });
    const initialCalls = getSessionCalls();

    await userEvent.type(geminiKeyInput, "a".repeat(24));
    await userEvent.click(screen.getAllByRole("button", { name: /^save$/i })[0]);

    await screen.findByText(/saved/i);
    await waitFor(() => expect(getSessionCalls()).toBeGreaterThan(initialCalls));
  });

  it("refreshes the session after successfully removing the key", async () => {
    mockAiSettings({
      ...BASE_SETTINGS,
      providers: { ...BASE_SETTINGS.providers, gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }) },
      provider_order: ["gemini"],
    });
    const getSessionCalls = trackSessionRefetches();
    server.use(
      http.post(`${BASE}/api/organization/ai`, () => HttpResponse.json(BASE_SETTINGS)),
    );

    renderPage();
    await screen.findByText("Remove key");
    const initialCalls = getSessionCalls();

    await userEvent.click(screen.getByText("Remove key"));
    const removeButtons = await screen.findAllByRole("button", { name: /^remove key$/i });
    await userEvent.click(removeButtons[removeButtons.length - 1]);

    await waitFor(() => expect(screen.queryByText("Remove Gemini key?")).not.toBeInTheDocument());
    await waitFor(() => expect(getSessionCalls()).toBeGreaterThan(initialCalls));
  });

  it("shows an inline error and keeps the dialog open when removing a key fails", async () => {
    mockAiSettings({
      ...BASE_SETTINGS,
      providers: { ...BASE_SETTINGS.providers, gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }) },
      provider_order: ["gemini"],
    });
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    await screen.findByText("Remove key");

    await userEvent.click(screen.getByText("Remove key"));
    const removeButtons = await screen.findAllByRole("button", { name: /^remove key$/i });
    await userEvent.click(removeButtons[removeButtons.length - 1]);

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
    expect(screen.getByText("Remove Gemini key?")).toBeInTheDocument();
  });

  it("shows an inline error when reordering providers fails", async () => {
    // The first non-primary row's "up" arrow is disabled, so swap the third row instead.
    mockAiSettings({
      ...BASE_SETTINGS,
      providers: {
        gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }),
        openai: providerInfo("gpt-5.6-terra", ["gpt-5.6-terra"], { key_configured: true, key_source: "org" }),
        anthropic: providerInfo("claude-sonnet-5", ["claude-sonnet-5"], { key_configured: true, key_source: "org" }),
      },
      provider_order: ["gemini", "openai", "anthropic"],
    });
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    const moveUp = await screen.findByRole("button", { name: /move anthropic up/i });
    await userEvent.click(moveUp);

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });

  it("shows an inline error when toggling fallback fails", async () => {
    mockAiSettings({
      ...BASE_SETTINGS,
      providers: {
        ...BASE_SETTINGS.providers,
        gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }),
        openai: providerInfo("gpt-5.6-terra", ["gpt-5.6-terra"], { key_configured: true, key_source: "org" }),
      },
      provider_order: ["gemini", "openai"],
    });
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    const fallbackSwitch = (await screen.findAllByRole("switch"))[0];
    await userEvent.click(fallbackSwitch);

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });

  it("keeps a typed rate-limit value in the input after an unrelated mutation (fallback toggle) succeeds", async () => {
    mockAiSettings({
      ...BASE_SETTINGS,
      providers: {
        ...BASE_SETTINGS.providers,
        gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }),
        openai: providerInfo("gpt-5.6-terra", ["gpt-5.6-terra"], { key_configured: true, key_source: "org" }),
      },
      provider_order: ["gemini", "openai"],
    });
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        // Fallback flipped but the same (null) rate_limit_org_value: must not wipe the typed input.
        HttpResponse.json({
          ...BASE_SETTINGS,
          providers: {
            ...BASE_SETTINGS.providers,
            gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }),
            openai: providerInfo("gpt-5.6-terra", ["gpt-5.6-terra"], { key_configured: true, key_source: "org" }),
          },
          provider_order: ["gemini", "openai"],
          fallback_enabled: true,
        }),
      ),
    );

    renderPage();
    const rateLimitInput = await screen.findByLabelText("AI feature calls per user, per minute");
    await userEvent.type(rateLimitInput, "42");
    expect(rateLimitInput).toHaveValue(42);

    const fallbackSwitch = screen.getAllByRole("switch")[0];
    await userEvent.click(fallbackSwitch);

    await waitFor(() => expect(fallbackSwitch).toHaveAttribute("aria-checked", "true"));
    expect(rateLimitInput).toHaveValue(42);
  });

  it("keeps a typed rate-limit value in the input after an unrelated mutation (reorder) succeeds", async () => {
    mockAiSettings({
      ...BASE_SETTINGS,
      providers: {
        gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }),
        openai: providerInfo("gpt-5.6-terra", ["gpt-5.6-terra"], { key_configured: true, key_source: "org" }),
        anthropic: providerInfo("claude-sonnet-5", ["claude-sonnet-5"], { key_configured: true, key_source: "org" }),
      },
      provider_order: ["gemini", "openai", "anthropic"],
    });
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        HttpResponse.json({
          ...BASE_SETTINGS,
          providers: {
            gemini: providerInfo("gemini-3.1-flash-lite", ["gemini-3.1-flash-lite"], { key_configured: true, key_source: "org" }),
            openai: providerInfo("gpt-5.6-terra", ["gpt-5.6-terra"], { key_configured: true, key_source: "org" }),
            anthropic: providerInfo("claude-sonnet-5", ["claude-sonnet-5"], { key_configured: true, key_source: "org" }),
          },
          provider_order: ["gemini", "anthropic", "openai"],
        }),
      ),
    );

    renderPage();
    const rateLimitInput = await screen.findByLabelText("AI feature calls per user, per minute");
    await userEvent.type(rateLimitInput, "42");
    expect(rateLimitInput).toHaveValue(42);

    const moveUp = await screen.findByRole("button", { name: /move anthropic up/i });
    await userEvent.click(moveUp);

    await waitFor(() => expect(screen.getByRole("button", { name: /move openai up/i })).toBeInTheDocument());
    expect(rateLimitInput).toHaveValue(42);
  });

  it("re-seeds the rate-limit input from the server value after a successful save", async () => {
    mockAiSettings(BASE_SETTINGS);
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        HttpResponse.json({ ...BASE_SETTINGS, rate_limit_org_value: 42 }),
      ),
    );

    renderPage();
    const rateLimitInput = await screen.findByLabelText("AI feature calls per user, per minute");
    await userEvent.type(rateLimitInput, "42");
    const saveButtons = screen.getAllByRole("button", { name: /^save$/i });
    const saveButton = saveButtons.find((b) => !b.hasAttribute("disabled"))!;
    await userEvent.click(saveButton);

    await screen.findByText("Saved.");
    expect(await screen.findByText("Organization override: 42/min")).toBeInTheDocument();
    expect(rateLimitInput).toHaveValue(42);
  });

  it("announces the rate-limit save result and clears it on the next edit", async () => {
    mockAiSettings(BASE_SETTINGS);
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        HttpResponse.json({ ...BASE_SETTINGS, rate_limit_org_value: 42 }),
      ),
    );

    renderPage();
    const rateLimitInput = await screen.findByLabelText("AI feature calls per user, per minute");
    await userEvent.type(rateLimitInput, "42");
    const saveButtons = screen.getAllByRole("button", { name: /^save$/i });
    const saveButton = saveButtons.find((b) => !b.hasAttribute("disabled"))!;
    await userEvent.click(saveButton);

    const message = await screen.findByText("Saved.");
    expect(message).toHaveAttribute("aria-live", "polite");

    await userEvent.type(rateLimitInput, "1");
    expect(screen.queryByText("Saved.")).not.toBeInTheDocument();
  });

  it("clears the Saved message on its own after the transient window", async () => {
    mockAiSettings(BASE_SETTINGS);
    server.use(
      http.post(`${BASE}/api/organization/ai`, () =>
        HttpResponse.json({ ...BASE_SETTINGS, rate_limit_org_value: 42 }),
      ),
    );

    // Fake timers from before mount so the test controls the transient-message timeout.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderPage();
      const rateLimitInput = await screen.findByLabelText("AI feature calls per user, per minute");
      await userEvent.type(rateLimitInput, "42");
      const saveButtons = screen.getAllByRole("button", { name: /^save$/i });
      const saveButton = saveButtons.find((b) => !b.hasAttribute("disabled"))!;
      await userEvent.click(saveButton);

      await screen.findByText("Saved.");

      await act(async () => {
        await vi.advanceTimersByTimeAsync(5100);
      });

      expect(screen.queryByText("Saved.")).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("toggling sample values posts include_sample_values and reflects the response", async () => {
    mockAiSettings(BASE_SETTINGS);
    let body: unknown = null;
    server.use(
      http.post(`${BASE}/api/organization/ai`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({ ...BASE_SETTINGS, include_sample_values: false });
      }),
    );

    renderPage();
    const sw = await screen.findByRole("switch", { name: /include sample values/i });
    expect(sw).toHaveAttribute("aria-checked", "true");
    await userEvent.click(sw);

    await waitFor(() => expect(sw).toHaveAttribute("aria-checked", "false"));
    expect(body).toEqual({ include_sample_values: false });
  });
  describe("monthly budget", () => {
    const usageBody = (spent: number) => ({
      budget: { monthly_usd: 25, spent_month_usd: spent, resets_at: "2026-11-01T00:00:00Z" },
    });

    it("renders the stored cap and status lines", async () => {
      mockAiSettings({ ...BASE_SETTINGS, monthly_budget_usd: 25 });
      renderPage();
      expect(await screen.findByTestId("ai-budget-input")).toHaveValue(25);
      expect(screen.getByTestId("ai-budget-status")).toHaveTextContent("Current cap: $25.00 per month");
    });

    it("says no cap is set when blank, and sends null when a cap is cleared", async () => {
      mockAiSettings({ ...BASE_SETTINGS, monthly_budget_usd: 25 });
      let body: unknown = null;
      server.use(
        http.post(`${BASE}/api/organization/ai`, async ({ request }) => {
          body = await request.json();
          return HttpResponse.json({ ...BASE_SETTINGS, monthly_budget_usd: null });
        }),
      );
      renderPage();
      const input = await screen.findByTestId("ai-budget-input");
      await userEvent.clear(input);
      await userEvent.click(screen.getAllByRole("button", { name: /^save$/i }).find((b) => !b.hasAttribute("disabled"))!);
      await waitFor(() => expect(body).toEqual({ monthly_budget_usd: null }));
      expect(await screen.findByText("No cap set.")).toBeInTheDocument();
    });

    it("saves a numeric cap", async () => {
      mockAiSettings(BASE_SETTINGS);
      let body: unknown = null;
      server.use(
        http.post(`${BASE}/api/organization/ai`, async ({ request }) => {
          body = await request.json();
          return HttpResponse.json({ ...BASE_SETTINGS, monthly_budget_usd: 25 });
        }),
      );
      renderPage();
      await userEvent.type(await screen.findByTestId("ai-budget-input"), "25");
      await userEvent.click(screen.getAllByRole("button", { name: /^save$/i }).find((b) => !b.hasAttribute("disabled"))!);
      await waitFor(() => expect(body).toEqual({ monthly_budget_usd: 25 }));
      expect(await screen.findByText("Current cap: $25.00 per month")).toBeInTheDocument();
    });

    it.each(["0", "-5"])("blocks Save and flags %s as invalid", async (value) => {
      mockAiSettings(BASE_SETTINGS);
      renderPage();
      const input = await screen.findByTestId("ai-budget-input");
      await userEvent.type(input, value);
      expect(await screen.findByTestId("ai-budget-error")).toHaveTextContent(
        "Enter an amount greater than $0, or leave blank for no cap.",
      );
      expect(input).toHaveAttribute("aria-invalid", "true");
      expect(input.getAttribute("aria-describedby")).toContain("ai-budget-error");
      expect(screen.getAllByRole("button", { name: /^save$/i }).every((b) => b.hasAttribute("disabled"))).toBe(true);
    });

    it("shows spend and reset date, and hides the line when usage fails", async () => {
      mockAiSettings(BASE_SETTINGS);
      server.use(http.get(`${BASE}/api/organization/ai/usage`, () => HttpResponse.json(usageBody(3.2))));
      const { unmount } = renderPage();
      expect(await screen.findByTestId("ai-budget-spent")).toHaveTextContent("Spent this month: $3.20 · resets Nov 1");
      unmount();

      server.use(http.get(`${BASE}/api/organization/ai/usage`, () => HttpResponse.json({}, { status: 500 })));
      renderPage();
      await screen.findByTestId("ai-budget-status");
      expect(screen.queryByTestId("ai-budget-spent")).not.toBeInTheDocument();
    });
  });
});
