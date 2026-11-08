import { afterEach, describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse, delay } from "msw";
import { server } from "./msw-server";
import { extensions } from "@/extensions";
import { AuthProvider } from "@/lib/auth";

import { InviteAcceptPage } from "@/pages/InviteAcceptPage";
import { ForgotPasswordPage } from "@/pages/ForgotPasswordPage";
import { ResetPasswordPage } from "@/pages/ResetPasswordPage";

const BASE = "http://localhost:8080";

afterEach(() => {
  extensions.length = 0;
});

// ForgotPasswordPage/ResetPasswordPage never use useAuth, so no AuthProvider.
function renderPage(ui: React.ReactNode, path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path="/invitation" element={ui} />
        <Route path="/reset" element={ui} />
        <Route path="/forgot" element={ui} />
        <Route path="/login" element={<p>Login page</p>} />
      </Routes>
    </MemoryRouter>,
  );
}

// InviteAcceptPage signs in via useAuth, so it needs QueryClientProvider + AuthProvider and a "/"
// landing route.
function renderInvitePage(path: string, qc = new QueryClient({ defaultOptions: { queries: { retry: false } } })) {
  return { ...render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[path]}>
        <AuthProvider>
          <Routes>
            <Route path="/invitation" element={<InviteAcceptPage />} />
            <Route path="/" element={<p>Landing page</p>} />
            <Route path="/login" element={<p>Login page</p>} />
          </Routes>
        </AuthProvider>
      </MemoryRouter>
    </QueryClientProvider>,
  ), qc };
}

describe("InviteAcceptPage", () => {
  it("verifies the token and prefills email + name", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, ({ request }) => {
        const token = new URL(request.url).searchParams.get("token");
        expect(token).toBe("good-token");
        return HttpResponse.json({ email: "invitee@example.com", name: "Jane" });
      }),
    );

    renderInvitePage("/invitation?token=good-token");

    expect(
      await screen.findByText(/Setting up account for invitee@example.com/i),
    ).toBeInTheDocument();
    expect(screen.getByLabelText(/^name$/i)).toHaveValue("Jane");
  });

  it("shows the static password requirement hint, not a live checklist", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
    );

    renderInvitePage("/invitation?token=good-token");

    await screen.findByText(/Setting up account/i);
    expect(screen.getByText("At least 8 characters.")).toBeInTheDocument();
  });

  it("wraps the page in AuthShell so a deployment's extension shell applies", async () => {
    extensions.push({
      navItems: [],
      routes: [],
      authShellSlot: ({ children }) => <div data-testid="ext-shell">{children}</div>,
    });
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
    );

    renderInvitePage("/invitation?token=good-token");

    await screen.findByText(/Setting up account/i);
    expect(screen.getByTestId("ext-shell")).toBeInTheDocument();
  });

  it("shows the dead-link terminal state, not the raw backend message, when the token is invalid", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ message: "invalid or expired invitation token" }, { status: 400 }),
      ),
    );

    renderInvitePage("/invitation?token=bad-token");

    expect(
      await screen.findByRole("heading", { name: /this invitation link isn't valid/i }),
    ).toBeInTheDocument();
    expect(screen.queryByLabelText(/password/i)).not.toBeInTheDocument();
    expect(screen.queryByText(/expired/i)).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: /go to sign in/i })).toHaveAttribute("href", "/login");
  });

  it("shows the expiry-specific terminal state, distinct from the generic invalid state, when verify returns invitation_expired", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json(
          { message: "invitation link has expired", error_code: "invitation_expired" },
          { status: 400 },
        ),
      ),
    );

    renderInvitePage("/invitation?token=expired-token");

    expect(
      await screen.findByRole("heading", { name: /this invitation link has expired/i }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("heading", { name: /this invitation link isn't valid/i }),
    ).not.toBeInTheDocument();
    expect(screen.queryByLabelText(/password/i)).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: /go to sign in/i })).toHaveAttribute("href", "/login");
  });

  it("shows the expiry-specific terminal state when accept returns invitation_expired", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
      http.post(`${BASE}/api/users/invitation/accept`, () =>
        HttpResponse.json(
          { message: "invitation link has expired", error_code: "invitation_expired" },
          { status: 400 },
        ),
      ),
    );

    renderInvitePage("/invitation?token=good-token");

    await screen.findByText(/Setting up account/i);
    await userEvent.type(screen.getByLabelText(/^password$/i), "supersecret1");
    await userEvent.click(screen.getByRole("button", { name: /create account/i }));

    expect(
      await screen.findByRole("heading", { name: /this invitation link has expired/i }),
    ).toBeInTheDocument();
    expect(screen.queryByText("Landing page")).not.toBeInTheDocument();
  });

  it("moves focus to the terminal heading so a screen-reader user isn't left on a vanished submit button", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ message: "bad token" }, { status: 400 }),
      ),
    );

    renderInvitePage("/invitation?token=bad-token");

    const heading = await screen.findByRole("heading", { name: /this invitation link isn't valid/i });
    expect(heading).toHaveFocus();
  });

  it("shows the terminal state when the token is missing entirely", async () => {
    renderInvitePage("/invitation");

    expect(
      await screen.findByRole("heading", { name: /this invitation link isn't valid/i }),
    ).toBeInTheDocument();
    expect(screen.queryByLabelText(/password/i)).not.toBeInTheDocument();
  });

  it("a server error (not a real answer about the token) shows the retryable verify-failed state, distinct from invalid/expired", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ message: "Internal error" }, { status: 500 }),
      ),
    );

    renderInvitePage("/invitation?token=good-token");

    const heading = await screen.findByRole("heading", { name: /couldn't verify this invitation/i });
    expect(heading).toBeInTheDocument();
    expect(heading).toHaveFocus();
    expect(
      screen.queryByRole("heading", { name: /this invitation link isn't valid/i }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("heading", { name: /this invitation link has expired/i }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /try again/i })).toBeInTheDocument();
  });

  it("a network error on verify (no ApiError at all) also lands on the retryable verify-failed state", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () => HttpResponse.error()),
    );

    renderInvitePage("/invitation?token=good-token");

    expect(
      await screen.findByRole("heading", { name: /couldn't verify this invitation/i }),
    ).toBeInTheDocument();
  });

  it("Try again re-runs verify and recovers into the normal form once the server answers", async () => {
    let attempt = 0;
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () => {
        attempt += 1;
        if (attempt === 1) return HttpResponse.json({ message: "Internal error" }, { status: 500 });
        return HttpResponse.json({ email: "invitee@example.com", name: "Jane" });
      }),
    );

    renderInvitePage("/invitation?token=good-token");

    await userEvent.click(await screen.findByRole("button", { name: /try again/i }));

    expect(await screen.findByText(/Setting up account for invitee@example.com/i)).toBeInTheDocument();
    expect(attempt).toBe(2);
  });

  it("posts {token,name,password}, signs in, and lands on the resolved landing route", async () => {
    const accept = vi.fn();
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
      http.post(`${BASE}/api/users/invitation/accept`, async ({ request }) => {
        accept(await request.json());
        return HttpResponse.json({ id: 1 });
      }),
    );

    renderInvitePage("/invitation?token=good-token");

    await screen.findByText(/Setting up account/i);
    await userEvent.type(screen.getByLabelText(/^password$/i), "supersecret1");
    await userEvent.click(screen.getByRole("button", { name: /create account/i }));

    expect(await screen.findByText("Landing page")).toBeInTheDocument();
    expect(accept).toHaveBeenCalledWith({
      token: "good-token",
      name: "Jane",
      password: "supersecret1",
    });
  });

  it("purges every non-session cached query on accept, before the new session is read back — otherwise a prior identity's cached rows (e.g. dashboards) would render under the newly accepted one", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
      http.post(`${BASE}/api/users/invitation/accept`, () => HttpResponse.json({ id: 1 })),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    // The session query is exempt from the purge (AuthProvider observes it); seed both to prove
    // only one survives.
    qc.setQueryData(["dashboards"], { stale: "rows from the previous identity" });
    qc.setQueryData(["session"], { user: { id: 99 }, org: { id: 1 } });

    renderInvitePage("/invitation?token=good-token", qc);

    await screen.findByText(/Setting up account/i);
    await userEvent.type(screen.getByLabelText(/^password$/i), "supersecret1");
    await userEvent.click(screen.getByRole("button", { name: /create account/i }));

    await screen.findByText("Landing page");
    expect(qc.getQueryData(["dashboards"])).toBeUndefined();
  });

  it("shows the submitting label and disables the button, never re-enabling it on success", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
      http.post(`${BASE}/api/users/invitation/accept`, async () => {
        await delay(30);
        return HttpResponse.json({ id: 1 });
      }),
    );

    renderInvitePage("/invitation?token=good-token");

    await screen.findByText(/Setting up account/i);
    await userEvent.type(screen.getByLabelText(/^password$/i), "supersecret1");
    await userEvent.click(screen.getByRole("button", { name: /create account/i }));

    const submitting = await screen.findByRole("button", { name: /creating your account…/i });
    expect(submitting).toBeDisabled();

    expect(await screen.findByText("Landing page")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /create account/i })).not.toBeInTheDocument();
  });

  it("shows client-side validation for a too-short password without calling the API", async () => {
    const accept = vi.fn();
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
      http.post(`${BASE}/api/users/invitation/accept`, async ({ request }) => {
        accept(await request.json());
        return HttpResponse.json({ id: 1 });
      }),
    );

    renderInvitePage("/invitation?token=good-token");

    await screen.findByText(/Setting up account/i);
    await userEvent.type(screen.getByLabelText(/^password$/i), "short");
    await userEvent.click(screen.getByRole("button", { name: /create account/i }));

    const error = await screen.findByText("Password must be at least 8 characters.");
    expect(error).toBeInTheDocument();
    expect(accept).not.toHaveBeenCalled();

    const input = screen.getByLabelText(/^password$/i);
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(input.getAttribute("aria-describedby")).toContain(error.id);

    await userEvent.type(input, "eronough");
    expect(
      screen.queryByText("Password must be at least 8 characters."),
    ).not.toBeInTheDocument();
    expect(input).toHaveAttribute("aria-invalid", "false");
  });

  it("replaces the form with the terminal state on a 400 from the accept call itself", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
      http.post(`${BASE}/api/users/invitation/accept`, () =>
        HttpResponse.json({ message: "invalid or expired invitation token" }, { status: 400 }),
      ),
    );

    renderInvitePage("/invitation?token=good-token");

    await screen.findByText(/Setting up account/i);
    await userEvent.type(screen.getByLabelText(/^password$/i), "supersecret1");
    await userEvent.click(screen.getByRole("button", { name: /create account/i }));

    expect(
      await screen.findByRole("heading", { name: /this invitation link isn't valid/i }),
    ).toBeInTheDocument();
    expect(screen.queryByText("Landing page")).not.toBeInTheDocument();
  });

  it("displays a non-terminal server error inline when accept fails for a reason other than a bad token", async () => {
    server.use(
      http.get(`${BASE}/api/users/invitation/verify`, () =>
        HttpResponse.json({ email: "invitee@example.com", name: "Jane" }),
      ),
      http.post(`${BASE}/api/users/invitation/accept`, () =>
        HttpResponse.json({ message: "Internal error" }, { status: 500 }),
      ),
    );

    renderInvitePage("/invitation?token=good-token");

    await screen.findByText(/Setting up account/i);
    await userEvent.type(screen.getByLabelText(/^password$/i), "supersecret1");
    await userEvent.click(screen.getByRole("button", { name: /create account/i }));

    expect(await screen.findByText(/Internal error/i)).toBeInTheDocument();
    expect(screen.queryByText("Landing page")).not.toBeInTheDocument();
    expect(screen.getByLabelText(/^password$/i)).toBeInTheDocument();
  });
});

describe("ForgotPasswordPage", () => {
  it("shows a generic success message regardless of whether the account exists", async () => {
    server.use(
      http.post(`${BASE}/api/forgot_password`, () => new HttpResponse(null, { status: 204 })),
    );

    renderPage(<ForgotPasswordPage />, "/forgot");

    await userEvent.type(screen.getByLabelText(/email/i), "nobody@example.com");
    await userEvent.click(screen.getByRole("button", { name: /send reset link/i }));

    expect(
      await screen.findByText(/a reset link has been sent/i),
    ).toBeInTheDocument();
    expect(screen.queryByText(/no account/i)).not.toBeInTheDocument();
  });

  it("disables the submit button until an email is entered", async () => {
    renderPage(<ForgotPasswordPage />, "/forgot");

    const btn = screen.getByRole("button", { name: /send reset link/i });
    expect(btn).toBeDisabled();

    await userEvent.type(screen.getByLabelText(/email/i), "user@example.com");
    expect(btn).toBeEnabled();
  });

  it("surfaces a server error", async () => {
    server.use(
      http.post(`${BASE}/api/forgot_password`, () =>
        HttpResponse.json({ message: "Mailer unavailable" }, { status: 500 }),
      ),
    );

    renderPage(<ForgotPasswordPage />, "/forgot");

    await userEvent.type(screen.getByLabelText(/email/i), "user@example.com");
    await userEvent.click(screen.getByRole("button", { name: /send reset link/i }));

    expect(await screen.findByText(/Mailer unavailable/i)).toBeInTheDocument();
    expect(screen.queryByText(/reset link has been sent/i)).not.toBeInTheDocument();
  });

  it("wraps the page in AuthShell so a deployment's extension shell applies", () => {
    extensions.push({
      navItems: [],
      routes: [],
      authShellSlot: ({ children }) => <div data-testid="ext-shell">{children}</div>,
    });

    renderPage(<ForgotPasswordPage />, "/forgot");

    expect(screen.getByTestId("ext-shell")).toBeInTheDocument();
  });
});

describe("ResetPasswordPage", () => {
  it("shows an error and no form when the token is missing", () => {
    renderPage(<ResetPasswordPage />, "/reset");

    expect(screen.getByText(/Invalid or missing reset token/i)).toBeInTheDocument();
    expect(screen.queryByLabelText(/new password/i)).not.toBeInTheDocument();
  });

  it("wraps the page in AuthShell so a deployment's extension shell applies", () => {
    extensions.push({
      navItems: [],
      routes: [],
      authShellSlot: ({ children }) => <div data-testid="ext-shell">{children}</div>,
    });

    renderPage(<ResetPasswordPage />, "/reset?token=t");

    expect(screen.getByTestId("ext-shell")).toBeInTheDocument();
  });

  it("has no confirm-password field — one password-entry pattern with the invite page", () => {
    renderPage(<ResetPasswordPage />, "/reset?token=t");

    expect(screen.getByLabelText(/new password/i)).toBeInTheDocument();
    expect(screen.queryByLabelText(/confirm password/i)).not.toBeInTheDocument();
  });

  it("shows client-side validation for a too-short password without calling the API", async () => {
    const reset = vi.fn();
    server.use(
      http.post(`${BASE}/api/reset_password`, async ({ request }) => {
        reset(await request.json());
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage(<ResetPasswordPage />, "/reset?token=t");

    await userEvent.type(screen.getByLabelText(/new password/i), "short");
    await userEvent.click(screen.getByRole("button", { name: /set password/i }));

    expect(
      await screen.findByText("Password must be at least 8 characters."),
    ).toBeInTheDocument();
    expect(reset).not.toHaveBeenCalled();
  });

  it("posts {token,password}, shows a success confirmation, and navigates to /login when continuing", async () => {
    const reset = vi.fn();
    server.use(
      http.post(`${BASE}/api/reset_password`, async ({ request }) => {
        reset(await request.json());
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage(<ResetPasswordPage />, "/reset?token=t");

    await userEvent.type(screen.getByLabelText(/new password/i), "supersecret1");
    await userEvent.click(screen.getByRole("button", { name: /set password/i }));

    expect(await screen.findByText("Password Updated")).toBeInTheDocument();
    expect(reset).toHaveBeenCalledWith({ token: "t", password: "supersecret1" });
    expect(screen.queryByText("Login page")).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /continue to sign in/i }));

    expect(await screen.findByText("Login page")).toBeInTheDocument();
  });

  it("shows an error message when the token is expired (400)", async () => {
    server.use(
      http.post(`${BASE}/api/reset_password`, () =>
        HttpResponse.json({ message: "Reset token expired" }, { status: 400 }),
      ),
    );

    renderPage(<ResetPasswordPage />, "/reset?token=expired");

    await userEvent.type(screen.getByLabelText(/new password/i), "supersecret1");
    await userEvent.click(screen.getByRole("button", { name: /set password/i }));

    expect(await screen.findByText(/Reset token expired/i)).toBeInTheDocument();
    expect(screen.queryByText("Login page")).not.toBeInTheDocument();
  });

  it("toggles the password field between masked and revealed, swapping icon and aria-label", async () => {
    renderPage(<ResetPasswordPage />, "/reset?token=t");

    const input = screen.getByLabelText(/new password/i);
    expect(input).toHaveAttribute("type", "password");

    const toggle = screen.getByRole("button", { name: /show password/i });
    await userEvent.click(toggle);

    expect(input).toHaveAttribute("type", "text");
    expect(screen.getByRole("button", { name: /hide password/i })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^show password$/i })).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /hide password/i }));
    expect(input).toHaveAttribute("type", "password");
  });
});
