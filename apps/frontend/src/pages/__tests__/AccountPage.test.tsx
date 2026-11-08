import { describe, it, expect } from "vitest";
import { http, HttpResponse } from "msw";
import { server } from "@/__tests__/msw-server";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { PERMISSION_KEYS } from "@/lib/permissions";
import { AuthProvider } from "@/lib/auth";
import { AccountPage } from "@/pages/AccountPage";

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <MemoryRouter>
          <AccountPage />
        </MemoryRouter>
      </AuthProvider>
    </QueryClientProvider>,
  );
}

describe("AccountPage", () => {
  it("sets the document title", async () => {
    renderPage();
    await screen.findByText("test@example.com");
    expect(document.title).toBe("Account · arcodash");
  });

  it("renders the email as a full-opacity read-only field, not a placeholder-look-alike disabled input", async () => {
    renderPage();

    const value = await screen.findByText("test@example.com");
    expect(value.tagName).toBe("P");
    expect(value).toHaveClass("text-foreground");
    expect(value).not.toHaveClass("opacity-50");
    expect(screen.getByText("Contact an admin to change the email on your account.")).toBeInTheDocument();

    // Never a real `<input>` for this field — that's the anti-pattern being replaced.
    expect(screen.queryByDisplayValue("test@example.com")).not.toBeInTheDocument();
  });

  it("shows change-password by default", async () => {
    renderPage();
    await screen.findByRole("button", { name: "Change password" });
  });

  it("hides change-password when the server reports password auth disabled", async () => {
    server.use(
      http.get("http://localhost:8080/api/session", () =>
        HttpResponse.json({
          user: {
            id: 1,
            name: "Test User",
            email: "test@example.com",
            groups: [1],
            org_id: 1,
            is_invitation_pending: false,
            disabled_at: null,
            profile_image_url: null,
            details: {},
            created_at: "2025-01-01T00:00:00Z",
            updated_at: "2025-01-01T00:00:00Z",
          },
          org: { id: 1, name: "Test Org", slug: "test-org", settings: {} },
          permissions: [...PERMISSION_KEYS],
          is_admin: true,
          client_config: {
            disable_public_urls: false,
            saml_enabled: false,
            ldap_enabled: false,
            ai_sql_generation: false,
            ai_monitor_suggestions: false,
            password_auth_enabled: false,
          },
        }),
      ),
    );
    renderPage();
    await screen.findByText("test@example.com");
    expect(screen.queryByRole("button", { name: "Change password" })).toBeNull();
  });
});
