import type { ReactNode } from "react";
import { describe, it, expect, vi } from "vitest";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";
import { MonitorPage } from "@/pages/MonitorPage";
import { api } from "@/lib/api";

// Stubbed to a shell rendering `monitorControls`, so page tests skip the section's own
// schedule/runs fetches.
vi.mock("@/components/scheduling/ScheduleSection", () => ({
  ScheduleSection: (props: { monitorControls?: ReactNode }) => (
    <div data-testid="schedule-section">{props.monitorControls}</div>
  ),
  MONITOR_FLAVOR: {},
}));

const BASE = "http://localhost:8080";

// `latest_query_data_id` set so the column field's never-run gate doesn't close over other tests.
const QUERY = { id: 7, name: "Signups query", latest_query_data_id: 555 };

function makeMonitor(overrides: Record<string, unknown> = {}) {
  return {
    id: 42,
    name: "Too many signups",
    query: QUERY,
    options: { column: "count", op: ">", value: 100, muted: false },
    state: "ok",
    last_triggered_at: null,
    rearm: null,
    user: { id: 1, name: "Test User", email: "test@example.com" },
    org_id: 1,
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
    caller: { object_role: "owner", can_edit: true, can_delete: true },
    ...overrides,
  };
}

function defaultHandlers(monitor = makeMonitor(), subs: unknown[] = [], deliveries: unknown[] = []) {
  return [
    http.get(`${BASE}/api/monitors/42`, () => HttpResponse.json(monitor)),
    http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
    http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json(subs)),
    http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json(deliveries)),
    http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
  ];
}

function renderPage(qc: QueryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })) {
  const result = render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <MemoryRouter initialEntries={["/monitors/42"]}>
          <Routes>
            <Route path="/monitors/:id" element={<MonitorPage />} />
            <Route path="/monitors" element={<p>Monitors list</p>} />
          </Routes>
        </MemoryRouter>
      </AuthProvider>
    </QueryClientProvider>,
  );
  return { ...result, qc };
}

describe("MonitorPage", () => {
  it("loads the monitor and populates op/value/column controls", async () => {
    server.use(...defaultHandlers());

    renderPage();

    expect(await screen.findByDisplayValue("Too many signups")).toBeInTheDocument();

    expect(await screen.findByPlaceholderText(/e\.g\. count/i)).toHaveValue("count");
    expect(screen.getByRole("spinbutton")).toHaveValue(100);

    expect(screen.getByText(">")).toBeInTheDocument();
  });

  it("titles the tab with the monitor's own name, not a static section noun", async () => {
    server.use(...defaultHandlers());

    renderPage();

    await screen.findByDisplayValue("Too many signups");
    expect(document.title).toBe("Too many signups · arcodash");
  });

  it("an archived monitor query's name renders as a full-opacity readonly value, not a disabled input", async () => {
    server.use(
      ...defaultHandlers(makeMonitor({ query: { id: 99, name: "Archived source query" } })),
      http.get(`${BASE}/api/queries/99/results`, () => new HttpResponse(null, { status: 404 })),
    );

    renderPage();

    const value = await screen.findByText("Archived source query");
    expect(value.tagName).toBe("P");
    expect(value).not.toBeDisabled();
    expect(value).not.toHaveClass("opacity-50");
  });

  it("shows the owner + visibility chips and opens ShareDialog from the Share button", async () => {
    server.use(
      ...defaultHandlers(makeMonitor({ visibility: "restricted" })),
      http.get(`${BASE}/api/monitors/42/acl`, () => HttpResponse.json([])),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getByText("Test User")).toBeInTheDocument();
    expect(screen.getByText("Specific people")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Share" }));
    expect(await screen.findByText("People & groups")).toBeInTheDocument();
  });

  it("hides Save/Delete/Share when caller.can_edit is false", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({ caller: { object_role: "viewer", can_edit: false, can_delete: false } }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.queryByRole("button", { name: /^save$/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^delete$/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Share" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^mute$/i })).not.toBeInTheDocument();
  });

  it("shows the read-only viewer notice and disables Name/Value when caller.can_edit is false", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({ caller: { object_role: "viewer", can_edit: false, can_delete: false } }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(
      screen.getByText("You don't have permission to edit this monitor. Viewing only."),
    ).toBeInTheDocument();
    expect(screen.getByPlaceholderText(/monitor name/i)).toBeDisabled();
    expect(screen.getByRole("spinbutton")).toBeDisabled();
  });

  it("hides the read-only notice and keeps Name/Value enabled when caller.can_edit is true", async () => {
    server.use(...defaultHandlers());

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(
      screen.queryByText(/you don't have permission to edit this monitor/i),
    ).not.toBeInTheDocument();
    expect(screen.getByPlaceholderText(/monitor name/i)).toBeEnabled();
    expect(screen.getByRole("spinbutton")).toBeEnabled();
  });

  it("keeps Subscribe and Acknowledge enabled for a viewer without edit access — those stay view-tier, not gated by canSave", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({
          state: "triggered",
          caller: { object_role: "viewer", can_edit: false, can_delete: false },
        }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getByRole("button", { name: /^subscribe$/i })).toBeEnabled();
    expect(screen.getByRole("button", { name: /^acknowledge$/i })).toBeEnabled();
  });

  it("hides Share when caller.object_role is editor even though can_edit is true (sharing is owner-tier)", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({ caller: { object_role: "editor", can_edit: true, can_delete: true } }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getByRole("button", { name: /^save$/i })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Share" })).not.toBeInTheDocument();
  });

  it("saving posts updated options to POST /api/monitors/:id", async () => {
    let posted: any = null;
    server.use(
      ...defaultHandlers(),
      http.post(`${BASE}/api/monitors/42`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(
          makeMonitor({ options: { column: "errors", op: ">", value: 5, muted: false } }),
        );
      }),
    );

    renderPage();

    // Wait for the populate effect, or its async setState races clear().
    await screen.findByDisplayValue("Too many signups");
    const columnInput = await screen.findByPlaceholderText(/e\.g\. count/i);
    await userEvent.clear(columnInput);
    await userEvent.type(columnInput, "errors");

    const valueInput = screen.getByRole("spinbutton");
    await userEvent.clear(valueInput);
    await userEvent.type(valueInput, "5");

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(posted).toEqual({
        name: "Too many signups",
        // Save merges the edit onto the freshly fetched row, so `muted` survives untouched.
        options: { column: "errors", op: ">", value: 5, muted: false, kind: "threshold", sensitivity: null },
        rearm: null,
      });
    });
  });

  it("save merges onto a fresh fetch, not the page-load cache — a mute set after this page loaded (e.g. from another tab) survives Save instead of being silently cleared", async () => {
    let posted: any = null;
    let getCalls = 0;
    server.use(
      // First GET is the mount load (unmuted); the save's own fetch returns muted, as if muted from
      // another tab. No `defaultHandlers()` spread: two GET handlers for one path have undefined
      // precedence.
      http.get(`${BASE}/api/monitors/42`, () => {
        getCalls += 1;
        return HttpResponse.json(
          makeMonitor({ options: { column: "count", op: ">", value: 100, muted: getCalls > 1 } }),
        );
      }),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.post(`${BASE}/api/monitors/42`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(makeMonitor({ options: posted.options }));
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    const valueInput = screen.getByRole("spinbutton");
    await userEvent.clear(valueInput);
    await userEvent.type(valueInput, "5");

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(posted).toEqual({
        name: "Too many signups",
        options: { column: "count", op: ">", value: 5, muted: true, kind: "threshold", sensitivity: null },
        rearm: null,
      });
    });
  });

  it("mute button calls POST /api/monitors/:id/mute and reflects muted state", async () => {
    let muteCalled = false;
    let getCount = 0;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () => {
        getCount += 1;
        return HttpResponse.json(
          makeMonitor({ options: { column: "count", op: ">", value: 100, muted: muteCalled } }),
        );
      }),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/monitors/42/mute`, () => {
        muteCalled = true;
        return HttpResponse.json(
          makeMonitor({ options: { column: "count", op: ">", value: 100, muted: true } }),
        );
      }),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["monitors"], []);
    renderPage(qc);

    const muteBtn = await screen.findByRole("button", { name: /^mute$/i });
    await userEvent.click(muteBtn);

    await waitFor(() => expect(muteCalled).toBe(true));
    expect(await screen.findByRole("button", { name: /^unmute$/i })).toBeInTheDocument();
    expect(getCount).toBeGreaterThan(1);
    // The monitors list cache is invalidated too, so client-side nav back isn't stale.
    expect(qc.getQueryState(["monitors"])?.isInvalidated).toBe(true);
  });

  it("unmute button calls POST /api/monitors/:id/unmute", async () => {
    let unmuteCalled = false;
    server.use(
      ...defaultHandlers(
        makeMonitor({ options: { column: "count", op: ">", value: 100, muted: true } }),
      ),
      http.post(`${BASE}/api/monitors/42/unmute`, () => {
        unmuteCalled = true;
        return HttpResponse.json(
          makeMonitor({ options: { column: "count", op: ">", value: 100, muted: false } }),
        );
      }),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["monitors"], []);
    renderPage(qc);

    const unmuteBtn = await screen.findByRole("button", { name: /^unmute$/i });
    await userEvent.click(unmuteBtn);

    await waitFor(() => expect(unmuteCalled).toBe(true));
    expect(qc.getQueryState(["monitors"])?.isInvalidated).toBe(true);
  });

  it("snooze dropdown renders the 3 duration items; picking one snoozes and shows the confirmed state", async () => {
    let snoozedUntil: string | null = null;
    let snoozeCalledWith: number | null = null;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () =>
        HttpResponse.json(
          makeMonitor({
            options: { column: "count", op: ">", value: 100, muted: false, muted_until: snoozedUntil },
          }),
        ),
      ),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/monitors/42/snooze`, async ({ request }) => {
        const body = (await request.json()) as { minutes: number };
        snoozeCalledWith = body.minutes;
        snoozedUntil = new Date(Date.now() + body.minutes * 60_000).toISOString();
        return HttpResponse.json(
          makeMonitor({
            options: { column: "count", op: ">", value: 100, muted: false, muted_until: snoozedUntil },
          }),
        );
      }),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["monitors"], []);
    renderPage(qc);
    await screen.findByDisplayValue("Too many signups");

    const snoozeTrigger = screen.getByRole("button", { name: /^snooze$/i });
    await userEvent.click(snoozeTrigger);
    expect(await screen.findByRole("menuitem", { name: /^1 hour$/i })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: /^4 hours$/i })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: /^1 day$/i })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("menuitem", { name: /^1 hour$/i }));

    await waitFor(() => expect(snoozeCalledWith).toBe(60));
    expect(await screen.findByRole("button", { name: /^cancel snooze$/i })).toBeInTheDocument();
    expect(await screen.findByText(/^Snoozed until .+ — notifications paused\.$/)).toBeInTheDocument();
    expect(qc.getQueryState(["monitors"])?.isInvalidated).toBe(true);
  });

  it('"Cancel snooze" calls snooze with minutes: 0 and the snoozed line disappears', async () => {
    let snoozedUntil: string | null = new Date(Date.now() + 60 * 60_000).toISOString();
    let snoozeCalledWith: number | null = null;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () =>
        HttpResponse.json(
          makeMonitor({
            options: { column: "count", op: ">", value: 100, muted: false, muted_until: snoozedUntil },
          }),
        ),
      ),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/monitors/42/snooze`, async ({ request }) => {
        const body = (await request.json()) as { minutes: number };
        snoozeCalledWith = body.minutes;
        snoozedUntil = body.minutes > 0 ? new Date(Date.now() + body.minutes * 60_000).toISOString() : null;
        return HttpResponse.json(
          makeMonitor({
            options: { column: "count", op: ">", value: 100, muted: false, muted_until: snoozedUntil },
          }),
        );
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    const cancelBtn = await screen.findByRole("button", { name: /^cancel snooze$/i });
    await userEvent.click(cancelBtn);

    await waitFor(() => expect(snoozeCalledWith).toBe(0));
    expect(await screen.findByRole("button", { name: /^snooze$/i })).toBeInTheDocument();
    expect(screen.queryByText(/Snoozed until/)).toBeNull();
  });

  it("snooze control is entirely absent when the monitor is muted", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({ options: { column: "count", op: ">", value: 100, muted: true } }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.queryByRole("button", { name: /^snooze$/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /^cancel snooze$/i })).toBeNull();
    expect(screen.getByRole("button", { name: /^unmute$/i })).toBeInTheDocument();
  });

  it("a muted monitor renders the muted status line", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({ options: { column: "count", op: ">", value: 100, muted: true } }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("Muted — notifications paused.")).toBeInTheDocument();
  });

  it("muted and snoozed status copy never render at the same time", async () => {
    const mutedUntil = new Date(Date.now() + 60 * 60_000).toISOString();
    server.use(
      ...defaultHandlers(
        makeMonitor({
          options: { column: "count", op: ">", value: 100, muted: true, muted_until: mutedUntil },
        }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("Muted — notifications paused.")).toBeInTheDocument();
    expect(screen.queryByText(/Snoozed until/)).toBeNull();
  });

  it("a snooze whose expiry passes flips the control back on its own, without a refetch", async () => {
    const mutedUntil = new Date(Date.now() + 15_000).toISOString();
    server.use(
      ...defaultHandlers(
        makeMonitor({ options: { column: "count", op: ">", value: 100, muted: false, muted_until: mutedUntil } }),
      ),
    );

    // Fake timers from before mount so useNow's setInterval is controlled; shouldAdvanceTime keeps
    // the initial fetch moving.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderPage();
      await screen.findByDisplayValue("Too many signups");
      expect(await screen.findByRole("button", { name: /^cancel snooze$/i })).toBeInTheDocument();

      await act(async () => {
        await vi.advanceTimersByTimeAsync(31_000);
      });

      expect(screen.queryByRole("button", { name: /^cancel snooze$/i })).toBeNull();
      expect(screen.getByRole("button", { name: /^snooze$/i })).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("subscribe with the default Email option posts destination_id: null", async () => {
    let posted: any = "unset";
    server.use(
      ...defaultHandlers(),
      http.post(`${BASE}/api/monitors/42/subscriptions`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json({
          id: 1,
          monitor_id: 42,
          user: { id: 1, name: "Test User", email: "test@example.com" },
          destination: null,
        });
      }),
    );

    renderPage();

    const subscribeBtn = await screen.findByRole("button", { name: /^subscribe$/i });
    await userEvent.click(subscribeBtn);

    await waitFor(() => {
      expect(posted).toEqual({ destination_id: null });
    });
  });

  it("subscribing via a selected destination posts destination_id as a number", async () => {
    let posted: any = "unset";
    server.use(
      ...defaultHandlers(),
      http.get(`${BASE}/api/destinations`, () =>
        HttpResponse.json([
          {
            id: 5,
            name: "Ops Webhook",
            type: "webhook",
            options: {},
            icon: "fa-bolt",
            created_at: "2025-01-01T00:00:00Z",
          },
        ]),
      ),
      http.post(`${BASE}/api/monitors/42/subscriptions`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json({
          id: 2,
          monitor_id: 42,
          user: { id: 1, name: "Test User", email: "test@example.com" },
          destination: { id: 5, name: "Ops Webhook", type: "webhook" },
        });
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    // Radix combobox has no name-from-content; the destination select is the last of three.
    const comboboxes = await screen.findAllByRole("combobox");
    await userEvent.click(comboboxes[comboboxes.length - 1]);
    await userEvent.click(await screen.findByRole("option", { name: /webhook: ops webhook/i }));

    await userEvent.click(screen.getByRole("button", { name: /^subscribe$/i }));

    await waitFor(() => {
      expect(posted).toEqual({ destination_id: 5 });
    });
    expect(typeof posted.destination_id).toBe("number");
  });

  it("shows the empty-state destination hint only after a real zero-length load, not while loading", async () => {
    server.use(...defaultHandlers());

    renderPage();

    const links = await screen.findAllByRole("link", { name: /add a destination/i });
    expect(links).toHaveLength(1);
    expect(links[0]).toHaveAttribute("href", "/destinations");
  });

  it("does not show the empty-state destination hint while destinations are still loading", async () => {
    server.use(
      ...defaultHandlers(),
      http.get(`${BASE}/api/destinations`, () => new Promise(() => {})),
    );

    renderPage();

    await screen.findByDisplayValue("Too many signups");
    expect(screen.queryAllByRole("link", { name: /add a destination/i })).toHaveLength(0);
  });

  it("destinations-fetch error still allows subscribing via the default Email option", async () => {
    let posted: any = "unset";
    server.use(
      ...defaultHandlers(),
      http.get(`${BASE}/api/destinations`, () => HttpResponse.json({ message: "boom" }, { status: 500 })),
      http.post(`${BASE}/api/monitors/42/subscriptions`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json({
          id: 1,
          monitor_id: 42,
          user: { id: 1, name: "Test User", email: "test@example.com" },
          destination: null,
        });
      }),
    );

    renderPage();

    await screen.findByDisplayValue("Too many signups");
    expect(screen.queryAllByRole("link", { name: /add a destination/i })).toHaveLength(0);
    expect(screen.getByText("Email (just me)")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /^subscribe$/i }));

    await waitFor(() => {
      expect(posted).toEqual({ destination_id: null });
    });
  });

  it("subscribing when already subscribed (409) settles quietly instead of showing an error", async () => {
    server.use(
      ...defaultHandlers(),
      http.post(`${BASE}/api/monitors/42/subscriptions`, () =>
        HttpResponse.json({ message: "already subscribed to this monitor" }, { status: 409 }),
      ),
    );

    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: /^subscribe$/i }));

    await waitFor(() => {
      expect(screen.queryByText(/already subscribed/i)).toBeNull();
      expect(screen.queryByText(/subscribe failed/i)).toBeNull();
    });
    expect(screen.getByText("Email (just me)")).toBeInTheDocument();
  });

  it("custom subject/body fields are optional: form submits fine when left empty", async () => {
    let posted: any = null;
    server.use(
      ...defaultHandlers(),
      http.post(`${BASE}/api/monitors/42`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(makeMonitor());
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(posted).toEqual({
        name: "Too many signups",
        options: { column: "count", op: ">", value: 100, muted: false, kind: "threshold", sensitivity: null },
        rearm: null,
      });
    });
  });

  it("custom subject/body values round-trip: typed values are posted on save", async () => {
    let posted: any = null;
    server.use(
      ...defaultHandlers(),
      http.post(`${BASE}/api/monitors/42`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(makeMonitor());
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    // user-event treats a literal "{" as a special-key token; double the opening brace.
    await userEvent.type(
      screen.getByPlaceholderText(/e\.g\. \{\{name\}\} is \{\{state\}\}/i),
      "{{{{name}} is {{{{state}}",
    );
    await userEvent.type(
      screen.getByPlaceholderText(/e\.g\. \{\{name\}\} transitioned to \{\{state\}\}/i),
      "Body for {{{{name}}",
    );

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(posted.options.custom_subject).toBe("{{name}} is {{state}}");
      expect(posted.options.custom_body).toBe("Body for {{name}}");
    });
  });

  it("custom subject/body values round-trip: a loaded monitor pre-fills the fields", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({
          options: {
            column: "count",
            op: ">",
            value: 100,
            muted: false,
            custom_subject: "{{name}} is {{state}}",
            custom_body: "Body for {{name}}",
          },
        }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByDisplayValue("{{name}} is {{state}}")).toBeInTheDocument();
    expect(await screen.findByDisplayValue("Body for {{name}}")).toBeInTheDocument();
  });

  it("renders a validation error (400 invalid op) without crashing", async () => {
    server.use(
      ...defaultHandlers(),
      http.post(`${BASE}/api/monitors/42`, () =>
        HttpResponse.json({ message: "invalid op" }, { status: 400 }),
      ),
    );

    renderPage();

    await screen.findByDisplayValue("Too many signups");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    expect(await screen.findByText(/invalid op/i)).toBeInTheDocument();
    expect(screen.getByPlaceholderText(/monitor name/i)).toBeInTheDocument();
  });

  it("refuses to save when the monitor's editable fields changed since it was opened, and issues no update", async () => {
    let getCalls = 0;
    let updateCalled = false;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () => {
        getCalls += 1;
        // Call 1 seeds the snapshot; later calls simulate a concurrent rename (`updated_at` left
        // unchanged on purpose).
        return HttpResponse.json(
          makeMonitor({ name: getCalls > 1 ? "Renamed by someone else" : "Too many signups" }),
        );
      }),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.post(`${BASE}/api/monitors/42`, () => {
        updateCalled = true;
        return HttpResponse.json(makeMonitor());
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    expect(await screen.findByTestId("monitor-conflict-banner")).toBeInTheDocument();
    expect(updateCalled).toBe(false);
  });

  it("a second save in the same session isn't blocked by its own first write", async () => {
    // Stateful mock so the second save's freshness check sees the row just written.
    let stored: Record<string, unknown> = makeMonitor();
    let updateCount = 0;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () => HttpResponse.json(stored)),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.post(`${BASE}/api/monitors/42`, async ({ request }) => {
        const body = (await request.json()) as {
          name: string;
          options: Record<string, unknown>;
          rearm: number | null;
        };
        updateCount += 1;
        stored = makeMonitor({
          name: body.name,
          options: body.options,
          rearm: body.rearm,
          updated_at: `2025-02-0${updateCount}T00:00:00Z`,
        });
        return HttpResponse.json(stored);
      }),
    );

    renderPage();
    const nameInput = await screen.findByPlaceholderText(/monitor name/i);
    await screen.findByDisplayValue("Too many signups");

    // A real edit moves the editable snapshot; without the onSuccess re-seed the second save would
    // 409.
    await userEvent.clear(nameInput);
    await userEvent.type(nameInput, "Renamed by me");

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() => expect(updateCount).toBe(1));
    expect(screen.queryByTestId("monitor-conflict-banner")).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() => expect(updateCount).toBe(2));
    expect(screen.queryByTestId("monitor-conflict-banner")).not.toBeInTheDocument();
  });

  it("a mute does not block a later Save", async () => {
    let muted = false;
    let updateCalled = false;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () =>
        // A mute bumps `updated_at` but touches no form field; the guard must ignore it.
        HttpResponse.json(
          makeMonitor({
            options: { column: "count", op: ">", value: 100, muted },
            updated_at: muted ? "2025-02-01T00:00:00Z" : "2025-01-01T00:00:00Z",
          }),
        ),
      ),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.post(`${BASE}/api/monitors/42/mute`, () => {
        muted = true;
        return HttpResponse.json(
          makeMonitor({
            options: { column: "count", op: ">", value: 100, muted: true },
            updated_at: "2025-02-01T00:00:00Z",
          }),
        );
      }),
      http.post(`${BASE}/api/monitors/42`, () => {
        updateCalled = true;
        return HttpResponse.json(makeMonitor({ options: { column: "count", op: ">", value: 100, muted: true } }));
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(await screen.findByRole("button", { name: /^mute$/i }));
    await screen.findByRole("button", { name: /^unmute$/i });

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => expect(updateCalled).toBe(true));
    expect(screen.queryByTestId("monitor-conflict-banner")).not.toBeInTheDocument();
  });

  it("a background updated_at bump with no editable-field change does not block Save", async () => {
    let updateCalled = false;
    server.use(
      // `updated_at`/`state`/`last_triggered_at` change per GET (worker tick); form-owned fields
      // don't.
      http.get(`${BASE}/api/monitors/42`, () =>
        HttpResponse.json(
          makeMonitor({ updated_at: new Date().toISOString(), state: "triggered", last_triggered_at: new Date().toISOString() }),
        ),
      ),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.post(`${BASE}/api/monitors/42`, () => {
        updateCalled = true;
        return HttpResponse.json(makeMonitor());
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => expect(updateCalled).toBe(true));
    expect(screen.queryByTestId("monitor-conflict-banner")).not.toBeInTheDocument();
  });

  it("the conflict banner offers Reload, and Reload rehydrates and unblocks Save", async () => {
    let getCalls = 0;
    let updateCalled = false;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () => {
        getCalls += 1;
        // Call 1: mount load. Call 2: blocked save's fetch (concurrent rename). Call 3+: Reload
        // refetch.
        return HttpResponse.json(makeMonitor({ name: getCalls > 1 ? "Renamed by someone else" : "Too many signups" }));
      }),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.post(`${BASE}/api/monitors/42`, () => {
        updateCalled = true;
        return HttpResponse.json(makeMonitor({ name: "Renamed by someone else" }));
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));
    await screen.findByTestId("monitor-conflict-banner");

    await userEvent.click(screen.getByRole("button", { name: /^reload$/i }));

    expect(await screen.findByDisplayValue("Renamed by someone else")).toBeInTheDocument();
    expect(screen.queryByTestId("monitor-conflict-banner")).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() => expect(updateCalled).toBe(true));
    expect(screen.queryByTestId("monitor-conflict-banner")).not.toBeInTheDocument();
  });

  it("a failed Reload surfaces the connection message and leaves the banner up", async () => {
    let getCalls = 0;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () => {
        getCalls += 1;
        if (getCalls > 2) return new HttpResponse(null, { status: 500 });
        return HttpResponse.json(makeMonitor({ name: getCalls > 1 ? "Renamed by someone else" : "Too many signups" }));
      }),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.post(`${BASE}/api/monitors/42`, () => HttpResponse.json(makeMonitor())),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));
    await screen.findByTestId("monitor-conflict-banner");

    await userEvent.click(screen.getByRole("button", { name: /^reload$/i }));

    expect(
      await screen.findByText(/couldn't reload this monitor\. check your connection and try again\./i),
    ).toBeInTheDocument();
    expect(screen.getByTestId("monitor-conflict-banner")).toBeInTheDocument();
    expect(screen.getByDisplayValue("Too many signups")).toBeInTheDocument();
  });

  it("shows a load-failure hint (not the run-the-query hint) and offers Retry on a 500 columns fetch", async () => {
    let columnCalls = 0;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () => HttpResponse.json(makeMonitor())),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => {
        columnCalls += 1;
        return HttpResponse.json({ message: "database unavailable" }, { status: 500 });
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText(/couldn't load result columns/i)).toBeInTheDocument();
    expect(screen.queryByText(/run the query at least once/i)).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /retry/i }));
    await waitFor(() => expect(columnCalls).toBeGreaterThan(1));
  });

  it("shows the run-the-query hint (not the load-failure hint) on a 404 columns fetch", async () => {
    server.use(...defaultHandlers());

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText(/run the query at least once to pick from its result columns/i)).toBeInTheDocument();
    expect(screen.queryByText(/couldn't load result columns/i)).not.toBeInTheDocument();
  });

  it("a query the list DTO marks as never-run issues no results request at all, and disables the column field", async () => {
    let resultsRequests = 0;
    const neverRunQuery = { id: 8, name: "Never run query", latest_query_data_id: null };
    server.use(
      http.get(`${BASE}/api/monitors/42`, () =>
        HttpResponse.json(makeMonitor({ query: neverRunQuery })),
      ),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([neverRunQuery])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/8/results`, () => {
        resultsRequests += 1;
        return new HttpResponse(null, { status: 404 });
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText(/run the query at least once to pick from its result columns/i)).toBeInTheDocument();
    expect(screen.getByPlaceholderText(/e\.g\. count/i)).toBeDisabled();
    expect(resultsRequests).toBe(0);
  });
});

describe("MonitorPage bad-id handling", () => {
  function renderAt(path: string) {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={[path]}>
            <Routes>
              <Route path="/monitors/:id" element={<MonitorPage />} />
              <Route path="/monitors" element={<p>Monitors list</p>} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );
  }

  it("a non-numeric route param never calls the API and shows the not-found card", async () => {
    const getMonitor = vi.spyOn(api, "getMonitor");
    server.use(
      http.get(`${BASE}/api/monitors/foo`, () =>
        HttpResponse.json({ message: "Invalid URL: Cannot parse `foo` to a `i64`" }, { status: 400 }),
      ),
    );

    renderAt("/monitors/foo");

    expect(await screen.findByText("Monitor not found")).toBeInTheDocument();
    expect(
      screen.getByText("This monitor doesn't exist or you don't have access to it."),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Cannot parse/i)).not.toBeInTheDocument();
    expect(getMonitor).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole("link", { name: /back to monitors/i }));
    expect(await screen.findByText("Monitors list")).toBeInTheDocument();

    getMonitor.mockRestore();
  });

  it("a numeric but nonexistent id (404) shows the same not-found card", async () => {
    server.use(
      http.get(`${BASE}/api/monitors/999`, () =>
        HttpResponse.json({ message: "not found" }, { status: 404 }),
      ),
      http.get(`${BASE}/api/monitors/999/subscriptions`, () =>
        HttpResponse.json({ message: "not found" }, { status: 404 }),
      ),
      http.get(`${BASE}/api/monitors/999/deliveries`, () =>
        HttpResponse.json({ message: "not found" }, { status: 404 }),
      ),
    );

    renderAt("/monitors/999");

    expect(await screen.findByText("Monitor not found")).toBeInTheDocument();
    expect(
      screen.getByText("This monitor doesn't exist or you don't have access to it."),
    ).toBeInTheDocument();
  });

  it("a real server error (500) is not mislabeled as not-found", async () => {
    server.use(
      http.get(`${BASE}/api/monitors/42`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
      http.get(`${BASE}/api/monitors/42/deliveries`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderAt("/monitors/42");

    expect(await screen.findByText("Couldn't load this monitor")).toBeInTheDocument();
    expect(screen.getByText("database unavailable")).toBeInTheDocument();
    expect(screen.queryByText("Monitor not found")).not.toBeInTheDocument();
  });
});

describe("MonitorPage Check-schedule section mount", () => {
  it("mounts the schedule-section for a saved monitor", async () => {
    server.use(...defaultHandlers());
    renderPage();

    await screen.findByDisplayValue("Too many signups");
    expect(await screen.findByTestId("schedule-section")).toBeInTheDocument();
  });

  it("does not mount the schedule-section in isNew mode", async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    server.use(http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])));
    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={["/monitors/new"]}>
            <Routes>
              <Route path="/monitors/new" element={<MonitorPage />} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );

    await screen.findByPlaceholderText(/monitor name/i);
    expect(screen.queryByTestId("schedule-section")).not.toBeInTheDocument();
  });

  it("the Notify control lives inside the schedule section, not the condition form", async () => {
    server.use(...defaultHandlers());
    renderPage();

    await screen.findByDisplayValue("Too many signups");
    const section = await screen.findByTestId("schedule-section");
    expect(
      within(section).getByText("Notify — how often you're told while this stays alerting"),
    ).toBeInTheDocument();
    expect(screen.queryByText(/^Notify$/)).not.toBeInTheDocument();
  });
});

describe("MonitorPage new-monitor prefill from ?query_id", () => {
  function renderNew(path: string) {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={[path]}>
            <Routes>
              <Route path="/monitors/new" element={<MonitorPage />} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );
  }

  it("pre-selects the query named by ?query_id", async () => {
    server.use(
      http.get(`${BASE}/api/queries`, () =>
        HttpResponse.json([QUERY, { id: 9, name: "Other query" }]),
      ),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
    );

    renderNew("/monitors/new?query_id=7");

    await screen.findByPlaceholderText(/monitor name/i);
    expect(await screen.findByText("Signups query")).toBeInTheDocument();
  });

  it("leaves the query picker unselected without a query_id param", async () => {
    server.use(http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])));

    renderNew("/monitors/new");

    await screen.findByPlaceholderText(/monitor name/i);
    expect(await screen.findByText("Select a query")).toBeInTheDocument();
  });
});

describe("MonitorPage delivery history panel", () => {
  function makeDelivery(overrides: Record<string, unknown> = {}) {
    return {
      id: 1,
      created_at: "2025-06-01T12:00:00Z",
      destination_id: null,
      subscription_id: null,
      state: "triggered",
      status: "sent",
      error: null,
      attempt_number: 1,
      ...overrides,
    };
  }

  it("renders sent/failed/retrying badges correctly", async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [], [
        makeDelivery({ id: 3, status: "sent", created_at: "2025-06-03T00:00:00Z" }),
        makeDelivery({ id: 2, status: "failed", error: "destination configuration error", created_at: "2025-06-02T00:00:00Z" }),
        makeDelivery({ id: 1, status: "retrying", created_at: "2025-06-01T00:00:00Z" }),
      ]),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("Sent")).toBeInTheDocument();
    expect(await screen.findByText("Failed")).toBeInTheDocument();
    expect(await screen.findByText("Retrying")).toBeInTheDocument();
    expect(screen.getByText("destination configuration error")).toBeInTheDocument();
  });

  it("shows attempt number only when greater than 1", async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [], [
        makeDelivery({ id: 2, status: "retrying", attempt_number: 2 }),
        makeDelivery({ id: 1, status: "sent", attempt_number: 1 }),
      ]),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("attempt 2")).toBeInTheDocument();
    expect(screen.getAllByText(/^attempt \d+$/)).toHaveLength(1);
  });

  it('"Last notified" reads only the latest sent row, not a more recent failed one', async () => {
    // Newest-first, like the real API.
    server.use(
      ...defaultHandlers(makeMonitor(), [], [
        makeDelivery({ id: 2, status: "failed", created_at: "2025-06-05T10:00:00Z", error: "destination configuration error" }),
        makeDelivery({ id: 1, status: "sent", created_at: "2025-06-01T08:00:00Z" }),
      ]),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    const expected = `Last notified ${new Date("2025-06-01T08:00:00Z").toLocaleString()}`;
    expect(await screen.findByText(expected)).toBeInTheDocument();
    expect(screen.queryByText(/^Last notified 6\/5\/2025/)).toBeNull();
    expect(screen.queryByText("Not notified yet")).toBeNull();
  });

  it('"Last notified" shows "Not notified yet" when there is no sent row, even with failures', async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [], [
        makeDelivery({ id: 1, status: "failed", error: "destination configuration error" }),
      ]),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("Not notified yet")).toBeInTheDocument();
  });

  it("shows the empty state when there are no deliveries", async () => {
    server.use(...defaultHandlers(makeMonitor(), [], []));

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("No notifications sent yet.")).toBeInTheDocument();
    expect(screen.getByText("Not notified yet")).toBeInTheDocument();
  });

  it('resolves a null destination_id to "Email"', async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [], [makeDelivery({ id: 1, destination_id: null })]),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("Email")).toBeInTheDocument();
  });

  it("resolves a known destination_id against the loaded destinations list", async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [], [
        makeDelivery({ id: 1, destination_id: 5, status: "sent" }),
      ]),
      http.get(`${BASE}/api/destinations`, () =>
        HttpResponse.json([
          {
            id: 5,
            name: "Ops Webhook",
            type: "webhook",
            options: {},
            icon: "fa-bolt",
            created_at: "2025-01-01T00:00:00Z",
          },
        ]),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("webhook: Ops Webhook")).toBeInTheDocument();
  });
});

describe("MonitorPage notify-on-subscribe UX", () => {
  function makeSub(overrides: Record<string, unknown> = {}) {
    return {
      id: 9,
      monitor_id: 42,
      user: { id: 1, name: "Test User", email: "test@example.com" },
      destination: null,
      ...overrides,
    };
  }

  it("shows the firing note when the monitor is triggered and not muted", async () => {
    server.use(...defaultHandlers(makeMonitor({ state: "triggered" })));

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(
      await screen.findByText(
        "This monitor is currently alerting. Anyone you subscribe is notified right away.",
      ),
    ).toBeInTheDocument();
  });

  it("hides the firing note when the monitor is ok", async () => {
    server.use(...defaultHandlers(makeMonitor({ state: "ok" })));

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(
      screen.queryByText(/currently alerting\. Anyone you subscribe is notified right away/i),
    ).toBeNull();
  });

  it("hides the firing note when the monitor is triggered but muted", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({ state: "triggered", options: { column: "count", op: ">", value: 100, muted: true } }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(
      screen.queryByText(/currently alerting\. Anyone you subscribe is notified right away/i),
    ).toBeNull();
  });

  it("hides the firing note when triggered but snoozed (not muted)", async () => {
    const future = new Date(Date.now() + 60 * 60_000).toISOString();
    server.use(
      ...defaultHandlers(
        makeMonitor({
          state: "triggered",
          options: { column: "count", op: ">", value: 100, muted: false, muted_until: future },
        }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(
      screen.queryByText(/currently alerting\. Anyone you subscribe is notified right away/i),
    ).toBeNull();
  });

  it('"Send now" posts to the notify endpoint and shows the queued confirmation', async () => {
    let notifyCalled = false;
    server.use(
      ...defaultHandlers(makeMonitor(), [makeSub()]),
      http.post(`${BASE}/api/monitors/42/subscriptions/9/notify`, () => {
        notifyCalled = true;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(await screen.findByRole("button", { name: /^send now$/i }));

    await waitFor(() => expect(notifyCalled).toBe(true));
    expect(
      await screen.findByText("Notification queued — the result appears in Delivery history below."),
    ).toBeInTheDocument();
  });

  it('"Send now" shows an inline destructive error on failure and it is local to the section', async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [makeSub()]),
      http.post(`${BASE}/api/monitors/42/subscriptions/9/notify`, () =>
        HttpResponse.json({ message: "boom" }, { status: 500 }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(await screen.findByRole("button", { name: /^send now$/i }));

    const errText = await screen.findByText("boom");
    expect(errText).toHaveClass("text-destructive");
    expect(screen.getByPlaceholderText(/monitor name/i)).toBeInTheDocument();
  });

  it('"Send now" 404 shows the "monitor no longer exists" copy', async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [makeSub()]),
      http.post(`${BASE}/api/monitors/42/subscriptions/9/notify`, () =>
        HttpResponse.json({ message: "not found" }, { status: 404 }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(await screen.findByRole("button", { name: /^send now$/i }));

    expect(await screen.findByText("This monitor no longer exists.")).toBeInTheDocument();
  });

  it("auto-clears the queued-confirmation line after ~5s", async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [makeSub()]),
      http.post(`${BASE}/api/monitors/42/subscriptions/9/notify`, () => new HttpResponse(null, { status: 204 })),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(await screen.findByRole("button", { name: /^send now$/i }));
    await screen.findByText("Notification queued — the result appears in Delivery history below.");

    await waitFor(
      () =>
        expect(
          screen.queryByText("Notification queued — the result appears in Delivery history below."),
        ).toBeNull(),
      { timeout: 6000 },
    );
  }, 10000);

  it("Subscriptions section shows the empty state when nobody is subscribed", async () => {
    server.use(...defaultHandlers());

    const { container } = renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getByText("No subscriptions yet")).toBeInTheDocument();
    expect(
      screen.getByText("Subscribe a person or destination above to get notified when this monitor triggers."),
    ).toBeInTheDocument();
    expect(container.querySelector("svg.lucide-bell")).toBeInTheDocument();
  });


  it("Escalation section shows the empty state and a disabled Add step with no destinations", async () => {
    server.use(...defaultHandlers());

    const { container } = renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getByText("No escalation steps")).toBeInTheDocument();
    expect(
      screen.getByText(
        "Add a step to notify someone else if this monitor stays alerting and no one acknowledges it.",
      ),
    ).toBeInTheDocument();
    expect(container.querySelector("svg.lucide-workflow")).toBeInTheDocument();
    const addStepButton = screen.getByRole("button", { name: /\+ add step/i });
    expect(addStepButton).toBeDisabled();
    expect(addStepButton).toHaveAttribute("title", "Add a destination first");
    expect(getComputedStyle(addStepButton).pointerEvents).not.toBe("none");
  });

  it("adding a step and picking a destination enables Save, and Save PUTs the chain", async () => {
    let putBody: any = "unset";
    server.use(
      ...defaultHandlers(),
      http.get(`${BASE}/api/destinations`, () =>
        HttpResponse.json([
          { id: 5, name: "Ops Webhook", type: "webhook", options: {}, icon: "fa-bolt", created_at: "2025-01-01T00:00:00Z" },
        ]),
      ),
      http.put(`${BASE}/api/monitors/42/escalation`, async ({ request }) => {
        putBody = await request.json();
        return HttpResponse.json(putBody);
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    const saveEscalationButton = screen.getByRole("button", { name: /^save escalation steps$/i });
    expect(saveEscalationButton).toBeEnabled();

    await userEvent.click(await screen.findByRole("button", { name: /\+ add step/i }));

    expect(saveEscalationButton).toBeDisabled();

    // Comboboxes in page order: query, operator, subscribe destination, new escalation destination.
    const comboboxes = await screen.findAllByRole("combobox");
    await userEvent.click(comboboxes[comboboxes.length - 1]);
    await userEvent.click(await screen.findByRole("option", { name: /webhook: ops webhook/i }));

    expect(saveEscalationButton).toBeEnabled();
    await userEvent.click(saveEscalationButton);

    await waitFor(() => {
      expect(putBody).toEqual([{ delay_minutes: 30, destination_id: 5 }]);
    });
    expect(await screen.findByText("Escalation steps saved.")).toBeInTheDocument();
  });

  it("disables every escalation control and hides the Save button for a non-editor caller", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({
          caller: { object_role: "viewer", can_edit: false, can_delete: false },
          escalation: [{ delay_minutes: 15, destination_id: 5 }],
        }),
      ),
      http.get(`${BASE}/api/destinations`, () =>
        HttpResponse.json([
          { id: 5, name: "Ops Webhook", type: "webhook", options: {}, icon: "fa-bolt", created_at: "2025-01-01T00:00:00Z" },
        ]),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    const delayInput = screen.getAllByDisplayValue("15")[0];
    expect(delayInput).toBeDisabled();
    expect(screen.getByRole("button", { name: "Move step up" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Move step down" })).toBeDisabled();
    expect(screen.getByRole("button", { name: /^remove$/i })).toBeDisabled();
    expect(screen.getByRole("button", { name: /\+ add step/i })).toBeDisabled();
    expect(screen.queryByRole("button", { name: /^save escalation steps$/i })).not.toBeInTheDocument();
  });

  it("Acknowledge is visible only while triggered and unacked; posting it invalidates and shows the acked line", async () => {
    let acked = false;
    server.use(
      http.get(`${BASE}/api/monitors/42`, () =>
        HttpResponse.json(
          makeMonitor(
            acked
              ? { state: "triggered", acked_at: "2025-06-01T12:00:00Z", acked_by_email: "acker@test.local" }
              : { state: "triggered" },
          ),
        ),
      ),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/monitors/42/ack`, () => {
        acked = true;
        return HttpResponse.json(
          makeMonitor({ state: "triggered", acked_at: "2025-06-01T12:00:00Z", acked_by_email: "acker@test.local" }),
        );
      }),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["monitors"], []);
    renderPage(qc);
    await screen.findByDisplayValue("Too many signups");

    const ackButton = screen.getByRole("button", { name: /^acknowledge$/i });
    await userEvent.click(ackButton);

    expect(await screen.findByText(/Acknowledged by acker@test\.local on/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^acknowledge$/i })).toBeNull();
    expect(qc.getQueryState(["monitors"])?.isInvalidated).toBe(true);
  });

  it("a 409 ack failure shows the not-triggered copy", async () => {
    server.use(
      ...defaultHandlers(makeMonitor({ state: "triggered" })),
      http.post(`${BASE}/api/monitors/42/ack`, () => HttpResponse.json({ message: "conflict" }, { status: 409 })),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(screen.getByRole("button", { name: /^acknowledge$/i }));

    expect(await screen.findByText("This monitor is no longer alerting.")).toBeInTheDocument();
  });

  it("shows the escalation progress line only while triggered, unacked, and a chain exists", async () => {
    server.use(
      ...defaultHandlers(
        makeMonitor({
          state: "triggered",
          escalation_step: 1,
          escalation: [
            { delay_minutes: 5, destination_id: 5 },
            { delay_minutes: 10, destination_id: 5 },
          ],
        }),
      ),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("Escalation: step 1 of 2 notified")).toBeInTheDocument();
  });
});

describe("MonitorPage rearm / re-notify frequency", () => {
  // Combobox order: [0] query, [1] operator, [2] rearm/notify, [3] subscribe destination.
  const REARM_SELECT_INDEX = 2;

  it("mode switch shows/hides the minutes input", async () => {
    server.use(...defaultHandlers());

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getAllByRole("spinbutton")).toHaveLength(1);

    const comboboxes = screen.getAllByRole("combobox");
    await userEvent.click(comboboxes[REARM_SELECT_INDEX]);
    await userEvent.click(await screen.findByRole("option", { name: /at most every n minutes/i }));

    expect(screen.getAllByRole("spinbutton")).toHaveLength(2);

    await userEvent.click(screen.getAllByRole("combobox")[REARM_SELECT_INDEX]);
    await userEvent.click(await screen.findByRole("option", { name: /each time it changes/i }));

    expect(screen.getAllByRole("spinbutton")).toHaveLength(1);
  });

  it('selecting "each time it changes" and saving sends rearm: null', async () => {
    let posted: any = null;
    server.use(
      ...defaultHandlers(makeMonitor({ rearm: 1800 })),
      http.post(`${BASE}/api/monitors/42`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(makeMonitor());
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    const comboboxes = screen.getAllByRole("combobox");
    await userEvent.click(comboboxes[REARM_SELECT_INDEX]);
    await userEvent.click(await screen.findByRole("option", { name: /each time it changes/i }));

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(posted.rearm).toBeNull();
    });
  });

  it('selecting "at most every N minutes" with 45 minutes and saving sends rearm: 2700', async () => {
    let posted: any = null;
    server.use(
      ...defaultHandlers(),
      http.post(`${BASE}/api/monitors/42`, async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(makeMonitor());
      }),
    );

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    const comboboxes = screen.getAllByRole("combobox");
    await userEvent.click(comboboxes[REARM_SELECT_INDEX]);
    await userEvent.click(await screen.findByRole("option", { name: /at most every n minutes/i }));

    const minutesInput = screen.getAllByRole("spinbutton")[1];
    await userEvent.clear(minutesInput);
    await userEvent.type(minutesInput, "45");

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(posted.rearm).toBe(2700);
    });
  });

  it("loading a monitor with rearm: 1800 pre-selects throttled mode with minutes 30", async () => {
    server.use(...defaultHandlers(makeMonitor({ rearm: 1800 })));

    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText("At most every N minutes")).toBeInTheDocument();
    const minutesInput = screen.getAllByRole("spinbutton")[1];
    expect(minutesInput).toHaveValue(30);
  });
});

describe("MonitorPage detail-page header and danger zone", () => {
  it("titles the page with the monitor's own name, not a generic 'Edit Monitor' label", async () => {
    server.use(...defaultHandlers(makeMonitor({ name: "Too many signups" })));
    renderPage();

    expect(await screen.findByRole("heading", { name: "Too many signups" })).toBeInTheDocument();
    expect(screen.queryByText("Edit Monitor")).not.toBeInTheDocument();
  });

  it("titles a fresh /monitors/new page 'New Monitor'", async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    server.use(http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])));
    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={["/monitors/new"]}>
            <Routes>
              <Route path="/monitors/new" element={<MonitorPage />} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );

    expect(await screen.findByRole("heading", { name: "New Monitor" })).toBeInTheDocument();
  });

  it("renders the state readout as plain status text, not a clickable action beside Share/Snooze/Mute", async () => {
    server.use(...defaultHandlers(makeMonitor({ state: "ok" })));
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getByText("OK")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^ok$/i })).not.toBeInTheDocument();
  });

  it("moves Delete into its own danger-zone section, separate from the Save button", async () => {
    server.use(...defaultHandlers());
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getByRole("heading", { name: "Danger zone" })).toBeInTheDocument();
    const deleteBtn = screen.getByRole("button", { name: "Delete monitor" });
    const saveBtn = screen.getByRole("button", { name: /^save$/i });
    expect(deleteBtn.parentElement).not.toBe(saveBtn.parentElement);
  });

  it("deleting from the danger zone confirms, then calls DELETE and returns to the list", async () => {
    let deleteCalled = false;
    server.use(
      ...defaultHandlers(),
      http.delete(`${BASE}/api/monitors/42`, () => {
        deleteCalled = true;
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(screen.getByRole("button", { name: "Delete monitor" }));
    await userEvent.click(await screen.findByRole("button", { name: /^delete$/i }));

    await waitFor(() => expect(deleteCalled).toBe(true));
    expect(await screen.findByText("Monitors list")).toBeInTheDocument();
  });
});

describe("MonitorPage delivery defaults", () => {
  function renderNewMonitor(destinations: unknown[] = []) {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/destinations`, () => HttpResponse.json(destinations)),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
    );
    return render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={["/monitors/new?query_id=7"]}>
            <Routes>
              <Route path="/monitors/new" element={<MonitorPage />} />
              <Route path="/monitors/:id" element={<p>Monitor detail</p>} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );
  }

  it("28a: the create form defaults to self-subscribed via Email, and Save subscribes the creator", async () => {
    let subscribePosted: any = "unset";
    server.use(
      http.post(`${BASE}/api/monitors`, () => HttpResponse.json(makeMonitor({ id: 99 }))),
      http.post(`${BASE}/api/monitors/99/subscriptions`, async ({ request }) => {
        subscribePosted = await request.json();
        return HttpResponse.json({
          id: 1,
          monitor_id: 99,
          user: { id: 1, name: "Test User", email: "test@example.com" },
          destination: null,
        });
      }),
    );
    renderNewMonitor();

    const notifyToggle = await screen.findByRole("switch");
    expect(notifyToggle).toHaveAttribute("data-state", "checked");
    expect(screen.getByText("Email (just me)")).toBeInTheDocument();

    // Fields start disabled until /api/session resolves canSave (create_monitor).
    const nameInput = await screen.findByPlaceholderText("Monitor name");
    await waitFor(() => expect(nameInput).toBeEnabled());
    await userEvent.type(nameInput, "New monitor");
    await userEvent.type(await screen.findByPlaceholderText(/e\.g\. count/i), "count");

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => expect(subscribePosted).toEqual({ destination_id: null }));
  });

  it("28a: switching the notify toggle off skips the subscribe call entirely", async () => {
    let subscribeCalled = false;
    server.use(
      http.post(`${BASE}/api/monitors`, () => HttpResponse.json(makeMonitor({ id: 99 }))),
      http.post(`${BASE}/api/monitors/99/subscriptions`, () => {
        subscribeCalled = true;
        return HttpResponse.json({});
      }),
    );
    renderNewMonitor();

    await userEvent.click(await screen.findByRole("switch"));
    expect(screen.queryByText("Email (just me)")).not.toBeInTheDocument();

    await userEvent.type(screen.getByPlaceholderText("Monitor name"), "New monitor");
    await userEvent.type(await screen.findByPlaceholderText(/e\.g\. count/i), "count");

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await screen.findByText("Monitor detail");
    expect(subscribeCalled).toBe(false);
  });

  it("28b: shows a no-subscribers banner on the detail page when there are zero subscriptions", async () => {
    server.use(...defaultHandlers(makeMonitor(), []));
    renderPage();

    expect(
      await screen.findByText("No one is subscribed — this monitor won't notify anyone when it triggers."),
    ).toBeInTheDocument();
  });

  it("28b: hides the no-subscribers banner once someone is subscribed", async () => {
    server.use(
      ...defaultHandlers(makeMonitor(), [
        { id: 9, monitor_id: 42, user: { id: 1, name: "Test User", email: "test@example.com" }, destination: null },
      ]),
    );
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.queryByText(/won't notify anyone/i)).not.toBeInTheDocument();
  });

  it("28b: the banner reappears live when the last subscriber is removed, not just on a fresh zero-sub load", async () => {
    let subs: unknown[] = [
      { id: 9, monitor_id: 42, user: { id: 1, name: "Test User", email: "test@example.com" }, destination: null },
    ];
    server.use(
      http.get(`${BASE}/api/monitors/42`, () => HttpResponse.json(makeMonitor())),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json(subs)),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
      http.delete(`${BASE}/api/monitor_subscriptions/9`, () => {
        subs = [];
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.queryByText(/won't notify anyone/i)).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /^remove$/i }));

    expect(
      await screen.findByText("No one is subscribed — this monitor won't notify anyone when it triggers."),
    ).toBeInTheDocument();
  });

  it("28a: surfaces the subscribe failure on the detail page immediately after create instead of only via the banner", async () => {
    server.use(
      http.post(`${BASE}/api/monitors`, () => HttpResponse.json(makeMonitor({ id: 99 }))),
      http.post(`${BASE}/api/monitors/99/subscriptions`, () =>
        HttpResponse.json({ message: "destination unreachable" }, { status: 500 }),
      ),
      http.get(`${BASE}/api/monitors/99`, () => HttpResponse.json(makeMonitor({ id: 99 }))),
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
      http.get(`${BASE}/api/monitors/99/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/99/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={["/monitors/new?query_id=7"]}>
            <Routes>
              <Route path="/monitors/new" element={<MonitorPage />} />
              <Route path="/monitors/:id" element={<MonitorPage />} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );

    const nameInput = await screen.findByPlaceholderText("Monitor name");
    await waitFor(() => expect(nameInput).toBeEnabled());
    await userEvent.type(nameInput, "New monitor");
    await userEvent.type(await screen.findByPlaceholderText(/e\.g\. count/i), "count");

    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    // Lands on the detail page with the subscribe failure surfaced immediately.
    expect(await screen.findByRole("heading", { name: "Too many signups" })).toBeInTheDocument();
    expect(await screen.findByText("destination unreachable")).toBeInTheDocument();
  });
});

const MIXED_TYPE_COLUMNS = [
  { name: "store", friendly_type: "string" },
  { name: "month", friendly_type: "date" },
  { name: "net_revenue", friendly_type: "float" },
  { name: "signup_count", friendly_type: "integer" },
  { name: "mystery", friendly_type: undefined },
];

function withColumnResult(monitor: Record<string, unknown>, columns: unknown[]) {
  return [
    http.get(`${BASE}/api/monitors/42`, () => HttpResponse.json(monitor)),
    http.get(`${BASE}/api/queries`, () => HttpResponse.json([QUERY])),
    http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
    http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
    http.get(`${BASE}/api/queries/7/results`, () =>
      HttpResponse.json({ query_result: { data: { columns, rows: [] } } }),
    ),
  ];
}

describe("MonitorPage column-type filtering", () => {
  it("only lists numeric columns in the picker, degrading gracefully for an unknown type_name", async () => {
    server.use(
      ...withColumnResult(makeMonitor({ options: { column: "signup_count", op: ">", value: 100 } }), MIXED_TYPE_COLUMNS),
    );
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    await userEvent.click(screen.getByTestId("monitor-column-select"));
    const listbox = await screen.findByRole("listbox");
    expect(within(listbox).getByText("net_revenue")).toBeInTheDocument();
    expect(within(listbox).getByText("signup_count")).toBeInTheDocument();
    expect(within(listbox).getByText("mystery")).toBeInTheDocument();
    expect(within(listbox).queryByText("store")).not.toBeInTheDocument();
    expect(within(listbox).queryByText("month")).not.toBeInTheDocument();
  });

  it("warns and blocks Save when the saved column is not numeric", async () => {
    server.use(
      ...withColumnResult(makeMonitor({ options: { column: "store", op: "==", value: 0 } }), MIXED_TYPE_COLUMNS),
    );
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText(/isn't a number column/i)).toBeInTheDocument();
    const saveBtn = screen.getByRole("button", { name: /^save$/i });
    expect(saveBtn).toBeDisabled();
    expect(saveBtn).toHaveAttribute("title", expect.stringContaining("numeric column"));
  });

  it("tells the user when none of a query's columns are numeric", async () => {
    server.use(
      ...withColumnResult(makeMonitor({ options: { column: "", op: ">", value: 0 } }), [
        { name: "store", friendly_type: "string" },
        { name: "region", friendly_type: "string" },
      ]),
    );
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(await screen.findByText(/none of this query's columns look numeric/i)).toBeInTheDocument();
  });
});

describe("MonitorPage first-row disclosure and condition preview", () => {
  it("renders the preview sentence and first-row disclosure once column and value are set", async () => {
    server.use(
      ...withColumnResult(
        makeMonitor({ options: { column: "net_revenue", op: "<", value: 20000 } }),
        MIXED_TYPE_COLUMNS,
      ),
    );
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    const expectedSentence = `Alerts when net_revenue in the first row is less than ${(20000).toLocaleString()}.`;
    expect(
      await screen.findByText((_, node) => node?.textContent === expectedSentence),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/only the first row of the result is checked/i),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: /learn more about how a check is evaluated/i }),
    ).toHaveAttribute(
      "href",
      "https://www.arcodash.com/docs/alerts-monitors/data-quality-monitors#how-a-check-is-evaluated",
    );
  });

  it("does not render the preview sentence before a column and a valid value are both set", async () => {
    server.use(
      ...withColumnResult(makeMonitor({ options: { column: "", op: ">", value: "" } }), MIXED_TYPE_COLUMNS),
    );
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.queryByText(/in the first row/)).not.toBeInTheDocument();
  });
});

describe("MonitorPage plain-language operators", () => {
  it("shows a plain-language phrase (not a raw symbol) for each operator option", async () => {
    server.use(...defaultHandlers());
    renderPage();
    await screen.findByDisplayValue("Too many signups");

    expect(screen.getByText("Comparison")).toBeInTheDocument();
    await userEvent.click(screen.getByTestId("monitor-operator-select"));
    const listbox = await screen.findByRole("listbox");
    expect(within(listbox).getByText("is greater than")).toBeInTheDocument();
    expect(within(listbox).getByText("is at least")).toBeInTheDocument();
    expect(within(listbox).getByText("does not equal")).toBeInTheDocument();
  });
});
