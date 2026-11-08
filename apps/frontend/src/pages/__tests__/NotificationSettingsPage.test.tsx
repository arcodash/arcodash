import { describe, it, expect } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "@/__tests__/msw-server";
import { AuthProvider } from "@/lib/auth";
import NotificationSettingsPage from "@/pages/NotificationSettingsPage";

const BASE = "http://localhost:8080";
// Same call the component makes, so assertions track the environment's zone.
const DETECTED_TZ = Intl.DateTimeFormat().resolvedOptions().timeZone;

function sessionUser(overrides: Record<string, unknown> = {}) {
  return {
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
    digest_enabled: false,
    digest_hour: null,
    digest_timezone: null,
    ...overrides,
  };
}

function mockSession(user: Record<string, unknown>) {
  server.use(
    http.get(`${BASE}/api/session`, () =>
      HttpResponse.json({
        user,
        org: { id: 1, name: "Test Org", slug: "test-org", settings: {} },
        permissions: [],
        is_admin: true,
      }),
    ),
  );
}

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <MemoryRouter initialEntries={["/settings/notifications"]}>
          <Routes>
            <Route path="/settings/notifications" element={<NotificationSettingsPage />} />
            <Route path="/settings" element={<p>Settings hub</p>} />
          </Routes>
        </MemoryRouter>
      </AuthProvider>
    </QueryClientProvider>,
  );
}

describe("NotificationSettingsPage", () => {
  it("hides the hour/timezone controls when the digest is off", async () => {
    mockSession(sessionUser({ digest_enabled: false }));
    renderPage();

    await waitFor(() => expect(screen.getByRole("switch")).not.toBeChecked());
    expect(screen.queryByLabelText("Send at")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Timezone")).not.toBeInTheDocument();
    expect(screen.queryByText(/you'll get your digest at/i)).not.toBeInTheDocument();
  });

  it("mounts the hour/timezone controls and the summary sentence when the digest is on", async () => {
    mockSession(
      sessionUser({ digest_enabled: true, digest_hour: 8, digest_timezone: "Europe/Berlin" }),
    );
    renderPage();

    await waitFor(() => expect(screen.getByRole("switch")).toBeChecked());
    expect(screen.getByLabelText("Send at")).toBeInTheDocument();
    expect(screen.getByLabelText("Timezone")).toBeInTheDocument();
    expect(
      screen.getByText("You'll get your digest at 08:00 each day, Europe/Berlin time."),
    ).toBeInTheDocument();
  });

  it("reflects a different hour in the summary sentence", async () => {
    mockSession(
      sessionUser({ digest_enabled: true, digest_hour: 17, digest_timezone: "America/New_York" }),
    );
    renderPage();

    await waitFor(() => expect(screen.getByRole("switch")).toBeChecked());
    expect(
      screen.getByText("You'll get your digest at 17:00 each day, America/New_York time."),
    ).toBeInTheDocument();
  });

  it("shows the detected-timezone hint only while the stored timezone still matches the detected zone", async () => {
    mockSession(
      sessionUser({ digest_enabled: true, digest_hour: 8, digest_timezone: DETECTED_TZ }),
    );
    renderPage();

    await waitFor(() => expect(screen.getByLabelText("Timezone")).toBeInTheDocument());
    expect(screen.getByText("Detected from your browser.")).toBeInTheDocument();
    expect(screen.getByLabelText("Timezone")).toHaveAttribute("aria-describedby", "digest-tz-hint");
  });

  it("hides the detected-timezone hint once the stored timezone differs from the detected zone", async () => {
    const other = DETECTED_TZ === "UTC" ? "Asia/Tokyo" : "UTC";
    mockSession(sessionUser({ digest_enabled: true, digest_hour: 8, digest_timezone: other }));
    renderPage();

    await waitFor(() => expect(screen.getByLabelText("Timezone")).toBeInTheDocument());
    expect(screen.queryByText("Detected from your browser.")).not.toBeInTheDocument();
    expect(screen.getByLabelText("Timezone")).not.toHaveAttribute("aria-describedby");
  });

  it("sends the default hour and the detected timezone together on first enable", async () => {
    mockSession(sessionUser({ digest_enabled: false, digest_hour: null, digest_timezone: null }));
    let capturedBody: Record<string, unknown> | null = null;
    server.use(
      http.post(`${BASE}/api/users/me/digest`, async ({ request }) => {
        capturedBody = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({
          enabled: true,
          hour: capturedBody.hour,
          timezone: capturedBody.timezone,
        });
      }),
    );
    renderPage();

    await waitFor(() => expect(screen.getByRole("switch")).not.toBeChecked());
    await userEvent.click(screen.getByRole("switch"));

    await waitFor(() => expect(capturedBody).not.toBeNull(), { timeout: 5000 });
    expect(capturedBody).toMatchObject({ enabled: true, hour: 8, timezone: DETECTED_TZ });
    expect(screen.getByLabelText("Send at")).toBeInTheDocument();
  });

  it("does not resend hour/timezone when merely toggling an already-configured digest off", async () => {
    mockSession(
      sessionUser({ digest_enabled: true, digest_hour: 8, digest_timezone: "Europe/Berlin" }),
    );
    let capturedBody: Record<string, unknown> | null = null;
    server.use(
      http.post(`${BASE}/api/users/me/digest`, async ({ request }) => {
        capturedBody = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({ enabled: false, hour: 8, timezone: "Europe/Berlin" });
      }),
    );
    renderPage();

    await waitFor(() => expect(screen.getByRole("switch")).toBeChecked());
    await userEvent.click(screen.getByRole("switch"));

    await waitFor(() => expect(capturedBody).not.toBeNull(), { timeout: 5000 });
    expect(capturedBody).toEqual({ enabled: false });
    expect(screen.queryByLabelText("Send at")).not.toBeInTheDocument();
  });

  it("does not render the detected timezone's label doubled", async () => {
    mockSession(
      sessionUser({ digest_enabled: true, digest_hour: 8, digest_timezone: DETECTED_TZ }),
    );
    renderPage();

    const trigger = await screen.findByLabelText("Timezone");
    expect(trigger).toHaveTextContent(DETECTED_TZ);
    expect(trigger.textContent).toBe(DETECTED_TZ);

    await userEvent.click(trigger);
    const listbox = await screen.findByRole("listbox");
    // The "All timezones" group must exclude the detected zone, which the "Detected" group already
    // lists.
    expect(within(listbox).getAllByText(DETECTED_TZ)).toHaveLength(1);
  });

  it("renders a back link to /settings above the page title", async () => {
    mockSession(sessionUser());
    renderPage();

    const back = await screen.findByRole("button", { name: "Back to settings" });
    await userEvent.click(back);
    expect(await screen.findByText("Settings hub")).toBeInTheDocument();
  });
});
