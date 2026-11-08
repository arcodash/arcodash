import { describe, it, expect, afterEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router";
import { http, HttpResponse } from "msw";
import { server } from "@/__tests__/msw-server";
import { AuthProvider } from "@/lib/auth";
import { extensions } from "@/extensions";
import { LoginPage } from "@/pages/LoginPage";

const BASE = "http://localhost:8080";

afterEach(() => {
  extensions.length = 0;
});

function renderLogin(clientConfig: Record<string, unknown>) {
  server.use(
    http.get(`${BASE}/api/session`, () =>
      HttpResponse.json({
        user: null,
        client_config: {
          disable_public_urls: false,
          saml_enabled: false,
          ldap_enabled: false,
          ai_sql_generation: false,
          ...clientConfig,
        },
      }),
    ),
  );
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/login"]}>
        <AuthProvider>
          <LoginPage />
        </AuthProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("LoginPage with password_auth_enabled", () => {
  it("renders only the loginSlot when disabled", async () => {
    extensions.push({ navItems: [], routes: [], loginSlot: () => <div>external sign-in</div> });
    renderLogin({ password_auth_enabled: false });
    await screen.findByText("external sign-in");
    expect(screen.queryByLabelText("Email")).toBeNull();
    expect(screen.queryByLabelText("Password")).toBeNull();
  });

  it("shows a not-configured message when disabled and no slot exists", async () => {
    renderLogin({ password_auth_enabled: false });
    await screen.findByText("Sign-in is not configured. Contact your administrator.");
    expect(screen.queryByLabelText("Email")).toBeNull();
  });

  it("renders the built-in form when the flag is absent", async () => {
    renderLogin({});
    await screen.findByLabelText("Email");
    expect(screen.getByLabelText("Password")).toBeInTheDocument();
    expect(screen.queryByText(/not configured/i)).toBeNull();
  });
});
