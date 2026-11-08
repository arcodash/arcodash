import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { ProviderCard } from "../ProviderCard";
import type { AiProviderInfo } from "@/types/api";

const NOW = new Date("2026-10-07T12:00:00Z");
const ago = (h: number) => new Date(NOW.getTime() - h * 3600_000).toISOString();

function renderCard(free_tier_seen_at: string | null) {
  const info: AiProviderInfo = {
    key_configured: true,
    key_source: "org",
    model: "m",
    base_url: null,
    default_model: "m",
    curated_models: ["m"],
    last_status: null,
    last_status_at: null,
    free_tier_seen_at,
  };
  render(
    <ProviderCard
      kind="gemini"
      label="Gemini"
      info={info}
      keyHelp=""
      saving={false}
      onSave={vi.fn()}
      testing={false}
      testResult={null}
      onTest={vi.fn()}
      onRequestRemove={vi.fn()}
    />,
  );
}

describe("ProviderCard free-tier callout", () => {
  afterEach(() => vi.useRealTimers());

  it("shows within 48h with the docs link", () => {
    vi.useFakeTimers({ now: NOW, toFake: ["Date"] });
    renderCard(ago(1));
    expect(screen.getByTestId("provider-free-tier-gemini")).toBeInTheDocument();
    expect(screen.getByText("This key is on Google's free tier")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /rate limits and billing/ })).toHaveAttribute(
      "href",
      "https://ai.google.dev/gemini-api/docs/rate-limits",
    );
    expect(screen.getByText("Last seen 1h ago.")).toBeInTheDocument();
  });

  it("is hidden after 48h", () => {
    vi.useFakeTimers({ now: NOW, toFake: ["Date"] });
    renderCard(ago(49));
    expect(screen.queryByTestId("provider-free-tier-gemini")).toBeNull();
  });

  it("is hidden when null", () => {
    renderCard(null);
    expect(screen.queryByTestId("provider-free-tier-gemini")).toBeNull();
  });
});
