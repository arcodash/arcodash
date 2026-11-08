import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { AppShell } from "@/components/AppShell";

const MALICIOUS_NAME = "<img src=x onerror=alert(1)>";

vi.mock("@/lib/auth", () => ({
  useAuth: () => ({
    user: { name: MALICIOUS_NAME, email: "a@x.com" },
    logout: vi.fn(),
  }),
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

describe("AppShell user display-name XSS guard", () => {
  it("renders a malicious user name as literal escaped text, never as injected DOM", async () => {
    renderShell();

    // The sidebar is collapsed in tests (initials only); expand it so the full name renders.
    await userEvent.click(screen.getByRole("button", { name: "Expand sidebar" }));

    // React escapes JSX text, so a text node with the raw payload exists; innerHTML injection would
    // have produced an <img> instead.
    expect(await screen.findByText(MALICIOUS_NAME)).toBeInTheDocument();

    expect(document.querySelector("img[onerror]")).toBeNull();
  });
});
