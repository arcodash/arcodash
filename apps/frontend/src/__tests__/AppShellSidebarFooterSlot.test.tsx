import { describe, it, expect, vi } from "vitest";
import { render, within } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { AppShell } from "@/components/AppShell";

vi.mock("@/lib/auth", () => ({
  useAuth: () => ({
    user: { name: "Ada Lovelace", email: "ada@example.com" },
    logout: vi.fn(),
  }),
}));

vi.mock("@/extensions", () => ({
  extensions: [{ sidebarFooterSlot: () => <div data-testid="footer-slot" /> }],
}));

function renderShell() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <AppShell />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("AppShell sidebar footer slot", () => {
  it("renders the slot between the nav and the account menu when an extension provides one", () => {
    const { container } = renderShell();

    const aside = container.querySelector("aside");
    const nav = aside!.querySelector("nav");
    const slot = aside!.querySelector('[data-testid="footer-slot"]');
    const accountTrigger = within(aside!).getByText("AL").closest("button");

    expect(nav).not.toBeNull();
    expect(slot).not.toBeNull();
    expect(accountTrigger).not.toBeNull();

    // DOCUMENT_POSITION_FOLLOWING: asserts ordering, not a specific depth/index, so an
    // unrelated structural addition to the sidebar doesn't retrigger this test.
    expect(nav!.compareDocumentPosition(slot!) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(
      slot!.compareDocumentPosition(accountTrigger!) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });
});
