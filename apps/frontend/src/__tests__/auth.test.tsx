import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Navigate, Route, Routes } from "react-router";
import { QueryCache, QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider, RequireAuth, RequirePermission, SESSION_QUERY_KEY, useAuth } from "@/lib/auth";
import { SWITCHABLE_ORGS_QUERY_KEY } from "@/lib/switchableOrgsQueryKey";
import { PERMISSION_KEYS } from "@/lib/permissions";
import { ApiError } from "@/lib/api";
import { AppShell } from "@/components/AppShell";
import type { SessionResponse } from "@/types/api";

const onLogoutSpy = vi.hoisted(() => vi.fn());
const onLogoutSpy2 = vi.hoisted(() => vi.fn());
vi.mock("@/extensions", () => ({
  extensions: [{ onLogout: onLogoutSpy }, { onLogout: onLogoutSpy2 }],
}));

const BASE = "http://localhost:8080";

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

/** Mirrors the production QueryClient defaults (lib/sessionExpiry.ts); the session-switch tests
 * depend on them. */
function makeProdClient() {
  return new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        refetchOnWindowFocus: false,
        staleTime: 30_000,
      },
    },
  });
}

function renderWithProviders(ui: React.ReactNode, initialPath = "/") {
  const qc = makeClient();
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[initialPath]}>
        {ui}
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function renderWithClient(ui: React.ReactNode, qc: QueryClient, initialPath = "/") {
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[initialPath]}>{ui}</MemoryRouter>
    </QueryClientProvider>,
  );
}

function LogoutButton() {
  const { logout } = useAuth();
  return <button onClick={() => void logout()}>Do logout</button>;
}

function LoginButton() {
  const { login } = useAuth();
  return (
    <button onClick={() => void login("test@example.com", "secret")}>Do login</button>
  );
}

function RefreshSessionButton() {
  const { refreshSession } = useAuth();
  return <button onClick={() => void refreshSession()}>Do refresh</button>;
}

function LoginButtonThatRedirectsOnSuccess() {
  const { login, user } = useAuth();
  if (user) {
    return <Navigate to="/" replace />;
  }
  return (
    <button onClick={() => void login("test@example.com", "secret")}>Do login</button>
  );
}

describe("RequireAuth", () => {
  it("redirects to /login when there is no user", async () => {
    server.use(
      http.get("http://localhost:8080/api/session", () =>
        HttpResponse.json({ user: null }),
      ),
    );

    renderWithProviders(
      <AuthProvider>
        <Routes>
          <Route
            path="/"
            element={
              <RequireAuth>
                <p>Protected content</p>
              </RequireAuth>
            }
          />
          <Route path="/login" element={<p>Login page</p>} />
        </Routes>
      </AuthProvider>,
    );

    expect(await screen.findByText("Login page")).toBeInTheDocument();
    expect(screen.queryByText("Protected content")).not.toBeInTheDocument();
  });

  it("renders children when a user is present", async () => {
    renderWithProviders(
      <AuthProvider>
        <Routes>
          <Route
            path="/"
            element={
              <RequireAuth>
                <p>Protected content</p>
              </RequireAuth>
            }
          />
          <Route path="/login" element={<p>Login page</p>} />
        </Routes>
      </AuthProvider>,
    );

    expect(await screen.findByText("Protected content")).toBeInTheDocument();
    expect(screen.queryByText("Login page")).not.toBeInTheDocument();
  });
});

describe("RequirePermission", () => {
  it("redirects to the computed landing page (never an error page) when the permission is missing", async () => {
    server.use(
      http.get("http://localhost:8080/api/session", () =>
        HttpResponse.json({
          user: { id: 1, name: "Viewer", email: "viewer@example.com" },
          org: { id: 1, name: "Org", slug: "org" },
          permissions: ["view_dashboard"],
          is_admin: false,
        }),
      ),
    );

    renderWithProviders(
      <AuthProvider>
        <Routes>
          <Route
            path="/"
            element={
              <RequireAuth>
                <RequirePermission anyOf={["admin"]}>
                  <p>Admin-only content</p>
                </RequirePermission>
              </RequireAuth>
            }
          />
          <Route path="/dashboards" element={<p>Dashboards landing</p>} />
        </Routes>
      </AuthProvider>,
    );

    expect(await screen.findByText("Dashboards landing")).toBeInTheDocument();
    expect(screen.queryByText("Admin-only content")).not.toBeInTheDocument();
    expect(screen.queryByText(/do not have permission/i)).not.toBeInTheDocument();
  });

  it("renders children when the permission is held", async () => {
    server.use(
      http.get("http://localhost:8080/api/session", () =>
        HttpResponse.json({
          user: { id: 1, name: "Admin", email: "admin@example.com" },
          org: { id: 1, name: "Org", slug: "org" },
          permissions: ["admin"],
          is_admin: true,
        }),
      ),
    );

    renderWithProviders(
      <AuthProvider>
        <Routes>
          <Route
            path="/"
            element={
              <RequireAuth>
                <RequirePermission anyOf={["admin"]}>
                  <p>Admin-only content</p>
                </RequirePermission>
              </RequireAuth>
            }
          />
        </Routes>
      </AuthProvider>,
    );

    expect(await screen.findByText("Admin-only content")).toBeInTheDocument();
  });
});

describe("logout", () => {
  it("clears cross-user cached data and re-seeds the session as logged out", async () => {
    server.use(http.post(`${BASE}/api/logout`, () => HttpResponse.json({})));

    const qc = makeClient();
    qc.setQueryData(["dashboards"], [{ id: 1 }]);

    renderWithClient(
      <AuthProvider>
        <LogoutButton />
      </AuthProvider>,
      qc,
    );

    await userEvent.click(screen.getByText("Do logout"));

    await waitFor(() => expect(qc.getQueryData(["dashboards"])).toBeUndefined());
    expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.user).toBeNull();
  });

  it("flips the still-mounted session observer to logged-out, without needing a remount", async () => {
    // logout() must not clear the QueryClient: that tears down the observed session query, so a
    // later setQueryData() lands on an unobserved one.
    server.use(http.post(`${BASE}/api/logout`, () => HttpResponse.json({})));

    const qc = makeClient();

    renderWithClient(
      <AuthProvider>
        <RequireAuth>
          <LogoutButton />
        </RequireAuth>
      </AuthProvider>,
      qc,
    );

    await userEvent.click(await screen.findByText("Do logout"));

    expect(qc.getQueryCache().find({ queryKey: SESSION_QUERY_KEY })?.observers.length).toBeGreaterThan(0);
    await waitFor(() => expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.user).toBeNull());
  });

  it("calls every registered extension's onLogout hook", async () => {
    server.use(http.post(`${BASE}/api/logout`, () => HttpResponse.json({})));
    onLogoutSpy.mockClear();

    const qc = makeClient();
    renderWithClient(
      <AuthProvider>
        <LogoutButton />
      </AuthProvider>,
      qc,
    );

    await userEvent.click(screen.getByText("Do logout"));

    await waitFor(() => expect(onLogoutSpy).toHaveBeenCalledTimes(1));
  });

  it("a throwing extension onLogout does not abort the rest of logout", async () => {
    server.use(http.post(`${BASE}/api/logout`, () => HttpResponse.json({})));
    onLogoutSpy.mockClear();
    onLogoutSpy2.mockClear();
    onLogoutSpy.mockImplementationOnce(() => {
      throw new Error("boom");
    });

    const qc = makeClient();
    qc.setQueryData(["dashboards"], [{ id: 1 }]);

    renderWithClient(
      <AuthProvider>
        <LogoutButton />
      </AuthProvider>,
      qc,
    );

    await userEvent.click(screen.getByText("Do logout"));

    await waitFor(() => expect(onLogoutSpy2).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(qc.getQueryData(["dashboards"])).toBeUndefined());
    expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.user).toBeNull();
  });

  it("still tears down local session state when the server logout call fails", async () => {
    server.use(http.post(`${BASE}/api/logout`, () => HttpResponse.json({ message: "boom" }, { status: 500 })));
    onLogoutSpy.mockClear();

    const qc = makeClient();
    qc.setQueryData(["dashboards"], [{ id: 1 }]);

    renderWithClient(
      <AuthProvider>
        <LogoutButton />
      </AuthProvider>,
      qc,
    );

    await userEvent.click(screen.getByText("Do logout"));

    await waitFor(() => expect(qc.getQueryData(["dashboards"])).toBeUndefined());
    expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.user).toBeNull();
    await waitFor(() => expect(onLogoutSpy).toHaveBeenCalledTimes(1));
  });
});

describe("login", () => {
  it("clears prior cached data but leaves the session query refetching with the new user", async () => {
    server.use(http.get(`${BASE}/api/session`, () => HttpResponse.json({ user: null })));

    const qc = makeClient();
    qc.setQueryData(["dashboards"], [{ id: 1 }]);

    renderWithClient(
      <AuthProvider>
        <LoginButton />
      </AuthProvider>,
      qc,
    );

    await waitFor(() =>
      expect(qc.getQueryData(SESSION_QUERY_KEY)).toMatchObject({ user: null }),
    );

    server.use(
      http.post(`${BASE}/api/login`, () =>
        HttpResponse.json({ id: 1, name: "Test User", email: "test@example.com" }),
      ),
      http.get(`${BASE}/api/session`, () =>
        HttpResponse.json({
          user: { id: 1, name: "Test User", email: "test@example.com" },
        }),
      ),
    );

    await userEvent.click(screen.getByText("Do login"));

    await waitFor(() => expect(qc.getQueryData(["dashboards"])).toBeUndefined());
    await waitFor(() =>
      expect(qc.getQueryData(SESSION_QUERY_KEY)).toMatchObject({
        user: expect.objectContaining({ id: 1, email: "test@example.com" }),
      }),
    );
  });

  it("lets a real RequireAuth-protected route become reachable after logging in from a logged-out session", async () => {
    let sessionCalls = 0;
    server.use(
      http.get(`${BASE}/api/session`, () => {
        sessionCalls += 1;
        return HttpResponse.json(
          sessionCalls === 1
            ? { user: null }
            : { user: { id: 1, name: "Test User", email: "test@example.com" } },
        );
      }),
      http.post(`${BASE}/api/login`, () =>
        HttpResponse.json({ id: 1, name: "Test User", email: "test@example.com" }),
      ),
    );

    renderWithProviders(
      <AuthProvider>
        <Routes>
          <Route
            path="/"
            element={
              <RequireAuth>
                <p>Protected content</p>
              </RequireAuth>
            }
          />
          <Route path="/login" element={<LoginButtonThatRedirectsOnSuccess />} />
        </Routes>
      </AuthProvider>,
    );

    expect(await screen.findByText("Do login")).toBeInTheDocument();

    await userEvent.click(screen.getByText("Do login"));

    expect(await screen.findByText("Protected content")).toBeInTheDocument();
  });
});

describe("refreshSession", () => {
  it("also invalidates the switchable-orgs query so a newly created/joined org shows up without a reload", async () => {
    const qc = makeClient();
    qc.setQueryData(SWITCHABLE_ORGS_QUERY_KEY, [{ id: 1, name: "Old Org" }]);

    renderWithClient(
      <AuthProvider>
        <RefreshSessionButton />
      </AuthProvider>,
      qc,
    );

    await userEvent.click(screen.getByText("Do refresh"));

    await waitFor(() =>
      expect(qc.getQueryState(SWITCHABLE_ORGS_QUERY_KEY)?.isInvalidated).toBe(true),
    );
  });
});

describe("global 401 handler", () => {
  // Mirrors sessionExpiry's QueryCache onError (not exported).
  function onError(err: unknown, query: { queryKey: readonly unknown[] }) {
    if (query.queryKey[0] === SESSION_QUERY_KEY[0]) return;
    if (err instanceof ApiError && err.status === 401) {
      qc.setQueryData<SessionResponse>(SESSION_QUERY_KEY, (old) =>
        old ? { ...old, user: null } : old,
      );
    }
  }

  let qc: QueryClient;

  function seedSession() {
    qc.setQueryData(SESSION_QUERY_KEY, {
      user: { id: 1, name: "X", email: "x@x.com" },
      org: null,
      client_config: null,
    });
  }

  it("forces the cached session's user to null on a 401 from a non-session query", () => {
    qc = new QueryClient({ queryCache: new QueryCache() });
    seedSession();

    onError(new ApiError(401, "unauthorized"), { queryKey: ["queries"] });

    expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.user).toBeNull();
  });

  it("leaves the session cache untouched when the 401 comes from the session query itself", () => {
    qc = new QueryClient({ queryCache: new QueryCache() });
    seedSession();
    const before = qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY);

    onError(new ApiError(401, "unauthorized"), { queryKey: SESSION_QUERY_KEY });

    expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)).toEqual(before);
  });

  it("leaves the session cache untouched for a non-401 error", () => {
    qc = new QueryClient({ queryCache: new QueryCache() });
    seedSession();
    const before = qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY);

    onError(new ApiError(500, "server error"), { queryKey: ["queries"] });

    expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)).toEqual(before);
  });
});

describe("session switch", () => {
  // Real AppShell nav/RequirePermission cycle: admin A logs out, limited B logs in, same mount.
  const CLIENT_CONFIG: SessionResponse["client_config"] = {
    disable_public_urls: false,
    saml_enabled: false,
    ldap_enabled: true,
    ai_sql_generation: false,
    ai_monitor_suggestions: false,
  };
  const SESSION_A: SessionResponse = {
    user: { id: 1, name: "Admin A", email: "a@example.com" },
    org: { id: 1, name: "Org A", slug: "org-a" },
    permissions: [...PERMISSION_KEYS],
    is_admin: true,
    client_config: CLIENT_CONFIG,
  };
  const SESSION_B: SessionResponse = {
    user: { id: 2, name: "Bea B", email: "b@example.com" },
    org: { id: 1, name: "Org A", slug: "org-a" },
    permissions: ["view_dashboard"],
    is_admin: false,
  };

  let currentSession: SessionResponse;

  /** Registers handlers once; state changes mutate `currentSession` so no in-flight request sees a
   * swapped handler. */
  function registerSwitchableSession() {
    currentSession = SESSION_A;
    server.use(
      http.get(`${BASE}/api/session`, () => HttpResponse.json(currentSession)),
      http.post(`${BASE}/api/logout`, () => {
        currentSession = { user: null };
        return HttpResponse.json({});
      }),
      http.post(`${BASE}/api/login`, async ({ request }) => {
        const body = (await request.json()) as { email: string; password: string };
        currentSession = body.email === SESSION_B.user!.email ? SESSION_B : SESSION_A;
        return HttpResponse.json(currentSession.user);
      }),
    );
  }

  function LoginAsTrigger({ email, password }: { email: string; password: string }) {
    const { login, user } = useAuth();
    if (user) return <Navigate to="/" replace />;
    return <button onClick={() => void login(email, password)}>Log in as {email}</button>;
  }

  /** One AuthProvider across both routes so logout/login never remounts it. */
  function renderSwitchHarness(qc: QueryClient) {
    return renderWithClient(
      <AuthProvider>
        <Routes>
          <Route
            path="/"
            element={
              <RequireAuth>
                <AppShell />
              </RequireAuth>
            }
          />
          <Route
            path="/login"
            element={<LoginAsTrigger email={SESSION_B.user!.email} password="anything" />}
          />
        </Routes>
      </AuthProvider>,
      qc,
    );
  }

  it("flips AppShell's nav/identity from A to B in the same mount, without a remount", async () => {
    registerSwitchableSession();
    const qc = makeProdClient();
    renderSwitchHarness(qc);

    const nav = await screen.findByRole("navigation");
    const linksA = await within(nav).findAllByRole("link");
    expect(linksA).toHaveLength(8);
    [
      "Queries",
      "Data Sources",
      "Dashboards",
      "Monitors",
      "Organization",
      "Access Control",
      "System Status",
      "Audit Log",
    ].forEach((label, i) => {
      expect(linksA[i]).toHaveAccessibleName(label);
    });
    expect(screen.getByText("AA")).toBeInTheDocument();

    await userEvent.click(screen.getByText("AA"));
    await userEvent.click(await screen.findByRole("menuitem", { name: /sign out/i }));

    await waitFor(() => expect(screen.queryByRole("navigation")).not.toBeInTheDocument());
    expect(screen.queryByText(SESSION_A.user!.email)).not.toBeInTheDocument();
    expect(screen.queryByText("Org A")).not.toBeInTheDocument();

    await userEvent.click(await screen.findByText(`Log in as ${SESSION_B.user!.email}`));

    const navB = await screen.findByRole("navigation");
    const linksB = await within(navB).findAllByRole("link");
    expect(linksB).toHaveLength(1);
    expect(linksB[0]).toHaveAccessibleName("Dashboards");

    // The sidebar trigger's name/email span stays mounted, duplicating the menu copy; scope to the
    // menu.
    await userEvent.click(screen.getByText("BB"));
    const menu = await screen.findByRole("menu");
    expect(within(menu).getByText(SESSION_B.user!.name)).toBeInTheDocument();
    expect(within(menu).getByText(SESSION_B.user!.email)).toBeInTheDocument();
    expect(screen.queryByText(SESSION_A.user!.email)).not.toBeInTheDocument();
  });

  function DashboardsProbe() {
    useQuery({ queryKey: ["dashboards"], queryFn: () => Promise.resolve([{ id: 1 }]) });
    return null;
  }

  it("holds nothing but the session query in cache after logout", async () => {
    registerSwitchableSession();
    const qc = makeProdClient();

    renderWithClient(
      <AuthProvider>
        <RequireAuth>
          <DashboardsProbe />
          <LogoutButton />
        </RequireAuth>
      </AuthProvider>,
      qc,
    );

    await waitFor(() => expect(qc.getQueryData(["dashboards"])).toEqual([{ id: 1 }]));

    await userEvent.click(await screen.findByText("Do logout"));

    await waitFor(() => expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.user).toBeNull());

    const keys = qc.getQueryCache().getAll().map((q) => q.queryKey);
    expect(keys.every((k) => k[0] === SESSION_QUERY_KEY[0])).toBe(true);
    expect(qc.getQueryData(["dashboards"])).toBeUndefined();
  });

  it("preserves client_config (app-level, not per-session) while clearing user/org/permissions/is_admin on logout", async () => {
    // logout() must not replace the session cache with `null`: that wipes client_config and
    // LoginPage shows its server-error card.
    registerSwitchableSession();
    const qc = makeProdClient();

    renderWithClient(
      <AuthProvider>
        <RequireAuth>
          <LogoutButton />
        </RequireAuth>
      </AuthProvider>,
      qc,
    );

    await waitFor(() =>
      expect(qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.client_config).toEqual(CLIENT_CONFIG),
    );

    await userEvent.click(await screen.findByText("Do logout"));

    await waitFor(() => {
      const session = qc.getQueryData<SessionResponse>(SESSION_QUERY_KEY);
      expect(session?.user).toBeNull();
      expect(session?.client_config).toEqual(CLIENT_CONFIG);
      expect(session?.org).toBeUndefined();
      expect(session?.permissions).toBeUndefined();
      expect(session?.is_admin).toBeUndefined();
    });
  });
});
