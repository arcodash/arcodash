import { describe, it, expect, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AppShell } from "@/components/AppShell";

const logoutSpy = vi.hoisted(() => vi.fn(() => Promise.resolve()));
vi.mock("@/lib/auth", () => ({
  useAuth: () => ({
    user: { name: "Ada Lovelace", email: "ada@example.com" },
    logout: logoutSpy,
  }),
}));

// usePermissions reads the real /api/session (default MSW handler: full admin).
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

describe("AppShell sidebar footer", () => {
  it("has no standalone Settings button, only a Settings item inside the account menu", async () => {
    renderShell();

    expect(screen.queryByRole("button", { name: "Settings" })).not.toBeInTheDocument();

    // Avatar initials render whether the sidebar is collapsed or expanded.
    await userEvent.click(screen.getByText("AL"));
    const settingsItem = await screen.findByRole("menuitem", { name: /settings/i });
    expect(settingsItem).toBeInTheDocument();
  });
});

describe("AppShell sidebar nav", () => {
  it("renders the 4 top-level items plus the admin section for a full-admin session, with no Destinations row (it lives under the Monitors tab strip)", async () => {
    renderShell();

    const nav = screen.getByRole("navigation");
    const links = await within(nav).findAllByRole("link");
    // Sidebar is collapsed in tests but labels stay mounted (opacity-0), so their text is readable.
    expect(links.map((l) => l.textContent)).toEqual([
      "Queries",
      "Data Sources",
      "Dashboards",
      "Monitors",
      "Organization",
      "Access Control",
      "System Status",
      "Audit Log",
    ]);
    expect(within(nav).queryByRole("link", { name: /destinations/i })).not.toBeInTheDocument();
  });
});

describe("AppShell accessible names", () => {
  it("gives every button a non-empty accessible name when collapsed", () => {
    renderShell();
    const buttons = screen.getAllByRole("button");
    expect(buttons.length).toBeGreaterThan(0);
    for (const button of buttons) {
      expect(button).toHaveAccessibleName();
    }
  });

  it("gives every button a non-empty accessible name when expanded", async () => {
    renderShell();
    await userEvent.click(screen.getByRole("button", { name: "Expand sidebar" }));
    const buttons = screen.getAllByRole("button");
    expect(buttons.length).toBeGreaterThan(0);
    for (const button of buttons) {
      expect(button).toHaveAccessibleName();
    }
  });

  it("labels the sidebar collapse toggle and swaps the label when toggled", async () => {
    renderShell();
    const toggle = screen.getByRole("button", { name: "Expand sidebar" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(toggle);
    const expanded = screen.getByRole("button", { name: "Collapse sidebar" });
    expect(expanded).toBeInTheDocument();
    expect(expanded).toHaveAttribute("aria-expanded", "true");
  });

  it("wraps collapsed nav icons in a tooltip carrying the nav label", async () => {
    renderShell();
    const queriesLink = await screen.findByRole("link", { name: "Queries" });
    await userEvent.hover(queriesLink);
    expect(await screen.findByRole("tooltip")).toHaveTextContent("Queries");
  });

  it("keeps every nav link's accessible name intact whether the sidebar is collapsed or expanded", async () => {
    renderShell();
    const labels = ["Queries", "Data Sources", "Dashboards", "Monitors"];

    for (const label of labels) {
      expect(await screen.findByRole("link", { name: label })).toHaveAccessibleName(label);
    }

    await userEvent.click(screen.getByRole("button", { name: "Expand sidebar" }));
    for (const label of labels) {
      expect(await screen.findByRole("link", { name: label })).toHaveAccessibleName(label);
    }
  });
});

describe("AppShell responsive/mobile nav", () => {
  it("collapses the sidebar by default when matchMedia reports a sub-1024px viewport", () => {
    const { container } = renderShell();
    const aside = container.querySelector("aside");
    expect(aside).toHaveClass("w-16");
  });

  it("omits the sidebar footer slot wrapper when no extension provides one (OSS build)", () => {
    const { container } = renderShell();
    const aside = container.querySelector("aside");
    const rail = aside!.firstElementChild;
    expect(rail!.children).toHaveLength(3);
  });

  it("renders a hamburger with an accessible label", () => {
    renderShell();
    expect(
      screen.getByRole("button", { name: "Open navigation" }),
    ).toBeInTheDocument();
  });

  it("opens the mobile nav drawer with nav links on hamburger click", async () => {
    renderShell();
    await userEvent.click(screen.getByRole("button", { name: "Open navigation" }));

    const dialog = await screen.findByRole("dialog");
    expect(await within(dialog).findByRole("link", { name: /queries/i })).toBeInTheDocument();
    expect(within(dialog).getByRole("link", { name: /dashboards/i })).toBeInTheDocument();
  });
});

describe("AppShell sign-out ordering", () => {
  it("navigates to /login before the logout request even resolves", async () => {
    // Pending logout: navigate() must run before `await logout()`.
    let resolveLogout!: () => void;
    logoutSpy.mockImplementation(
      () => new Promise<void>((resolve) => { resolveLogout = resolve; }),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={["/"]}>
          <Routes>
            <Route path="/" element={<AppShell />} />
            <Route path="/login" element={<p>Login page</p>} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );

    await userEvent.click(screen.getByText("AL"));
    await userEvent.click(await screen.findByRole("menuitem", { name: /sign out/i }));

    expect(await screen.findByText("Login page")).toBeInTheDocument();
    expect(logoutSpy).toHaveBeenCalled();

    resolveLogout();
  });

  it("still ends up signed out with no unhandled rejection when the server logout call fails", async () => {
    // Exercises the real logout(): handleLogout has no catch, so safety relies on logout() never
    // rejecting. Needs a fresh unmocked `@/lib/auth`.
    const BASE = "http://localhost:8080";
    server.use(
      http.post(`${BASE}/api/logout`, () => HttpResponse.json({ message: "boom" }, { status: 500 })),
    );

    vi.resetModules();
    vi.doUnmock("@/lib/auth");
    const { AuthProvider } = await import("@/lib/auth");
    const { AppShell: RealAppShell } = await import("@/components/AppShell");

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={["/"]}>
          <AuthProvider>
            <Routes>
              <Route path="/" element={<RealAppShell />} />
              <Route path="/login" element={<p>Login page</p>} />
            </Routes>
          </AuthProvider>
        </MemoryRouter>
      </QueryClientProvider>,
    );

    await userEvent.click(await screen.findByText("TU"));
    await userEvent.click(await screen.findByRole("menuitem", { name: /sign out/i }));

    expect(await screen.findByText("Login page")).toBeInTheDocument();
  });
});
