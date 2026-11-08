import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";
import type { Dashboard, Widget } from "@/types/api";

vi.mock("@/components/dashboard/DashboardGrid", () => ({
  DashboardGrid: ({ widgets }: { widgets: Widget[] }) => (
    <div data-testid="dashboard-grid">
      {widgets.map((w) => (
        <div key={w.id} data-testid={`widget-${w.id}`}>
          {w.text}
        </div>
      ))}
    </div>
  ),
}));

vi.mock("@/components/dashboard/DashboardParamsBar", () => ({
  DashboardParamsBar: () => <div data-testid="params-bar" />,
}));

vi.mock("@/components/dashboard/AddWidgetDialog", () => ({
  AddWidgetDialog: () => <div data-testid="add-widget-dialog" />,
}));

import { DashboardPage } from "@/pages/DashboardPage";

const BASE = "http://localhost:8080";

function makeWidget(id: number, text: string): Widget {
  return {
    id,
    dashboard_id: 1,
    visualization_id: null,
    text,
    width: 1,
    options: {},
    viewer_can_read_data: true,
    created_at: "",
    updated_at: "",
  };
}

function makeDashboard(overrides: Partial<Dashboard> = {}): Dashboard {
  return {
    id: 1,
    slug: "sales",
    name: "Sales Dashboard",
    user_id: 1,
    visibility: "workspace",
    layout: null,
    dashboard_filters_enabled: false,
    is_archived: false,
    published: true,
    has_unpublished_changes: false,
    tags: [],
    options: {},
    is_favorite: false,
    widgets: [makeWidget(10, "Revenue widget")],
    created_at: "",
    updated_at: "",
    caller: { object_role: "owner", can_edit: true, can_delete: true, can_share: true },
    ...overrides,
  };
}

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function renderDashboard(slug = "sales") {
  const qc = makeClient();
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <MemoryRouter initialEntries={[`/dashboards/${slug}`]}>
          <Routes>
            <Route path="/dashboards/:slug" element={<DashboardPage />} />
          </Routes>
        </MemoryRouter>
      </AuthProvider>
    </QueryClientProvider>,
  );
}

const originalMatchMedia = window.matchMedia;

afterEach(() => {
  window.matchMedia = originalMatchMedia;
});

function mockViewport(matches: boolean) {
  window.matchMedia = ((query: string) => ({
    matches,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
}

beforeEach(() => {
  Object.assign(navigator, {
    clipboard: { writeText: vi.fn().mockResolvedValue(undefined) },
  });
});

describe("DashboardPage", () => {
  it("renders the dashboard name and widgets from GET /api/dashboards/:slug", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards/sales`, () =>
        HttpResponse.json(makeDashboard()),
      ),
    );

    renderDashboard();

    expect(await screen.findByText("Sales Dashboard")).toBeInTheDocument();
    expect(await screen.findByTestId("widget-10")).toHaveTextContent(
      "Revenue widget",
    );
  });

  it("titles the tab with the dashboard's own name, not a static section noun", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
    );

    renderDashboard();

    await screen.findByText("Sales Dashboard");
    expect(document.title).toBe("Sales Dashboard · arcodash");
  });

  it("renders an error state when the dashboard fetch 404s", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards/missing`, () =>
        HttpResponse.json({ message: "Dashboard not found" }, { status: 404 }),
      ),
    );

    renderDashboard("missing");

    expect(await screen.findByText("Dashboard not found")).toBeInTheDocument();
    expect(screen.queryByText("Sales Dashboard")).not.toBeInTheDocument();
  });

  it("an auto-refresh tick re-fetches the dashboard itself, not just each tile's own result, so the freshness dot can actually advance", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      let dashboardFetches = 0;
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => {
          dashboardFetches += 1;
          return HttpResponse.json(makeDashboard());
        }),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");
      expect(dashboardFetches).toBe(1);

      // Radix combobox has no name-from-content; find it by text.
      const refreshTrigger = screen.getByText("Refresh: Off").closest('[data-slot="select-trigger"]')!;
      await userEvent.click(refreshTrigger);
      await userEvent.click(await screen.findByRole("option", { name: "10s" }));
      await screen.findByText("Refresh: 10s");

      await act(async () => {
        await vi.advanceTimersByTimeAsync(10_000);
      });

      await waitFor(() => expect(dashboardFetches).toBeGreaterThan(1));
    } finally {
      vi.useRealTimers();
    }
  });

  describe("share & embed dialog", () => {
    it("creates a public embed after the confirm checkbox is checked, and shows the returned public_url", async () => {
      let embedPosted = false;
      // Stateful mock: the embeds list refetches after create and must show the new row.
      let embeds: Array<Record<string, unknown>> = [];
      const created = {
        id: 1,
        object_type: "dashboard",
        object_id: 1,
        mode: "public",
        token: "tok-abc",
        public_url: "http://localhost:3000/public/embed/tok-abc",
        locked_params: {},
        expires_at: null,
        anon_authorized: true,
        revoked_at: null,
        created_at: "2026-01-01T00:00:00Z",
      };
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
        http.get(`${BASE}/api/embeds`, () => HttpResponse.json(embeds)),
        http.post(`${BASE}/api/embeds`, () => {
          embedPosted = true;
          embeds = [created];
          return HttpResponse.json(created);
        }),
      );

      renderDashboard();

      await userEvent.click(await screen.findByRole("button", { name: /public link/i }));
      await userEvent.click(
        await screen.findByRole("checkbox", {
          name: /publicly accessible to anyone with the link/i,
        }),
      );
      await userEvent.click(
        await screen.findByRole("button", { name: /create public link/i }),
      );

      const urlInput = await screen.findByDisplayValue(
        "http://localhost:3000/public/embed/tok-abc",
      );
      expect(urlInput).toHaveAttribute("readonly");
      expect(embedPosted).toBe(true);
    });

    it("revokes an active embed via DELETE and reverts to the create-link state", async () => {
      let deleted = false;
      // Stateful mock: the embeds list refetches after revoke and must reflect the DELETE.
      let embeds: Array<Record<string, unknown>> = [
        {
          id: 7,
          object_type: "dashboard",
          object_id: 1,
          mode: "public",
          token: "tok-existing",
          public_url: "http://localhost:3000/public/embed/tok-existing",
          locked_params: {},
          expires_at: null,
          anon_authorized: true,
          revoked_at: null,
          created_at: "2026-01-01T00:00:00Z",
        },
      ];
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
        http.get(`${BASE}/api/embeds`, () => HttpResponse.json(embeds)),
        http.delete(`${BASE}/api/embeds/7`, () => {
          deleted = true;
          embeds = [];
          return new HttpResponse(null, { status: 204 });
        }),
      );

      renderDashboard();

      await userEvent.click(await screen.findByRole("button", { name: /public link/i }));

      const revokeBtn = await screen.findByRole("button", { name: /revoke/i });
      await userEvent.click(revokeBtn);

      expect(deleted).toBe(true);
      expect(
        await screen.findByRole("button", { name: /^create public link$/i }),
      ).toBeInTheDocument();
    });
  });

  describe("share (access control) dialog", () => {
    it("opens the ShareDialog with People & groups and visibility controls from the Share action", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
        http.get(`${BASE}/api/dashboards/1/acl`, () => HttpResponse.json([])),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      await userEvent.click(screen.getByRole("button", { name: "Share" }));

      expect(await screen.findByText("People & groups")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Everyone" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Specific people" })).toBeInTheDocument();
    });
  });

  describe("caller-gated controls", () => {
    it("shows the Edit toggle, rename pencil, and Share button when caller.can_edit/can_share are true", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.getByRole("button", { name: "Edit" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Sales Dashboard" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Share" })).toBeInTheDocument();
    });

    it("hides the Edit toggle, rename pencil, and Share button when caller.can_edit/can_share are false", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(
            makeDashboard({
              caller: { object_role: "viewer", can_edit: false, can_delete: false, can_share: false },
            }),
          ),
        ),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.queryByRole("button", { name: "Edit" })).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Sales Dashboard" })).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "Share" })).not.toBeInTheDocument();
      expect(screen.getByText("Sales Dashboard")).toBeInTheDocument();
    });
  });

  describe("toolbar (ActionBar)", () => {
    it("Edit toggle flips to View and reveals Add Widget", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.queryByRole("button", { name: "Add Widget" })).not.toBeInTheDocument();

      await userEvent.click(screen.getByRole("button", { name: "Edit" }));

      expect(await screen.findByRole("button", { name: "View" })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Add Widget" })).toBeInTheDocument();
    });

    it("changes the refresh interval from the overflow menu", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.getByText("Refresh: Off")).toBeInTheDocument();

      await userEvent.click(screen.getByRole("button", { name: "More actions" }));
      await userEvent.click(await screen.findByRole("menuitem", { name: "1m" }));

      expect(await screen.findByText("Refresh: 1m")).toBeInTheDocument();
    });

    it("sizes the refresh control to its content instead of a fixed width that clips the label", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      const trigger = screen.getByRole("combobox");
      expect(trigger.className).toMatch(/\bw-fit\b/);
      expect(trigger.className).not.toMatch(/\bw-\d/);
    });
  });

  describe("empty widgets state", () => {
    it("shows an icon, explanation, and mode-appropriate action when there are no widgets", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(makeDashboard({ widgets: [] })),
        ),
      );

      const { container } = renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.getByText("Add your first widget")).toBeInTheDocument();
      expect(
        screen.getByText("Widgets turn a saved query into a chart or table on this dashboard."),
      ).toBeInTheDocument();
      expect(container.querySelector("svg.lucide-layout-dashboard")).toBeInTheDocument();
      const switchButton = screen.getByRole("button", { name: "Switch to Edit mode to add widgets" });
      expect(switchButton).toBeInTheDocument();

      await userEvent.click(switchButton);

      expect((await screen.findAllByRole("button", { name: /add widget/i })).length).toBeGreaterThan(0);
    });

    it("shows a quiet line with no icon or action for a viewer who cannot edit", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(
            makeDashboard({
              widgets: [],
              caller: { object_role: "viewer", can_edit: false, can_delete: false, can_share: false },
            }),
          ),
        ),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.getByText("No widgets have been added to this dashboard yet.")).toBeInTheDocument();
      expect(screen.queryByText("Add your first widget")).not.toBeInTheDocument();
    });
  });

  describe("rename", () => {
    it("a rename rejected by the server is not re-sent on the next blur", async () => {
      let patchCount = 0;
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
        http.post(`${BASE}/api/dashboards/sales`, () => {
          patchCount += 1;
          return HttpResponse.json({ message: "Name already taken" }, { status: 400 });
        }),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      await userEvent.click(screen.getByRole("button", { name: "Sales Dashboard" }));
      const input = await screen.findByDisplayValue("Sales Dashboard");
      await userEvent.clear(input);
      await userEvent.type(input, "Taken Name");

      fireEvent.blur(input);
      await screen.findByText("Name already taken");
      expect(patchCount).toBe(1);

      // A second blur with an unchanged input must not re-fire the rejected request.
      fireEvent.blur(input);
      await waitFor(() => expect(screen.getByDisplayValue("Taken Name")).toBeInTheDocument());
      expect(patchCount).toBe(1);
    });

    it("editing the name after a rejection re-arms the save", async () => {
      let patchCount = 0;
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
        http.post(`${BASE}/api/dashboards/sales`, () => {
          patchCount += 1;
          return HttpResponse.json({ message: "Name already taken" }, { status: 400 });
        }),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      await userEvent.click(screen.getByRole("button", { name: "Sales Dashboard" }));
      const input = await screen.findByDisplayValue("Sales Dashboard");
      await userEvent.clear(input);
      await userEvent.type(input, "Taken Name");
      fireEvent.blur(input);
      await screen.findByText("Name already taken");
      expect(patchCount).toBe(1);

      await userEvent.type(input, "!");
      fireEvent.blur(input);
      await waitFor(() => expect(patchCount).toBe(2));
    });
  });

  describe("favorite toggle", () => {
    it("stars via POST /api/dashboards/:id/favorite when not favorited", async () => {
      let isFavorite = false;
      let starred = false;
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(makeDashboard({ is_favorite: isFavorite })),
        ),
        http.post(`${BASE}/api/dashboards/1/favorite`, () => {
          starred = true;
          isFavorite = true;
          return new HttpResponse(null, { status: 204 });
        }),
      );

      renderDashboard();

      await screen.findByText("Sales Dashboard");
      await userEvent.click(screen.getByRole("button", { name: /favorite/i }));

      await waitFor(() => expect(starred).toBe(true));
    });

    it("unstars via DELETE /api/dashboards/:id/favorite when already favorited", async () => {
      let isFavorite = true;
      let unstarred = false;
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(makeDashboard({ is_favorite: isFavorite })),
        ),
        http.delete(`${BASE}/api/dashboards/1/favorite`, () => {
          unstarred = true;
          isFavorite = false;
          return new HttpResponse(null, { status: 204 });
        }),
      );

      renderDashboard();

      await screen.findByText("Sales Dashboard");
      await userEvent.click(screen.getByRole("button", { name: /favorite/i }));

      await waitFor(() => expect(unstarred).toBe(true));
    });
  });

  describe("draft vs published toggle", () => {
    it("Edit mode requests ?draft=true and shows the draft widget set; View mode requests no draft param and shows published", async () => {
      const seenDraftParams: (string | null)[] = [];
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, ({ request }) => {
          const draftParam = new URL(request.url).searchParams.get("draft");
          seenDraftParams.push(draftParam);
          const widgets =
            draftParam === "true"
              ? [makeWidget(10, "Revenue widget"), makeWidget(11, "New draft widget")]
              : [makeWidget(10, "Revenue widget")];
          return HttpResponse.json(makeDashboard({ widgets, published: true }));
        }),
      );

      renderDashboard();

      expect(await screen.findByTestId("widget-10")).toBeInTheDocument();
      expect(screen.queryByTestId("widget-11")).not.toBeInTheDocument();
      expect(seenDraftParams).toContain(null);

      await userEvent.click(screen.getByRole("button", { name: "Edit" }));

      expect(await screen.findByTestId("widget-11")).toBeInTheDocument();
      expect(seenDraftParams).toContain("true");
    });

    it("shows widgets in default View mode for a never-published dashboard, not the empty state", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(
            makeDashboard({ published: false, widgets: [makeWidget(10, "Revenue widget")] }),
          ),
        ),
      );

      renderDashboard();

      expect(await screen.findByTestId("widget-10")).toHaveTextContent("Revenue widget");
      expect(screen.queryByText("Add your first widget")).not.toBeInTheDocument();
    });
  });

  describe("lifecycle badges, banners, confirms", () => {
    it("shows the Draft badge, the never-published banner, a disabled Publish with no tiles, and no Restore/history UI", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(
            makeDashboard({ published: false, has_unpublished_changes: false, widgets: [] }),
          ),
        ),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.getByText("Draft")).toBeInTheDocument();
      expect(screen.queryByText("Unpublished changes")).not.toBeInTheDocument();

      await userEvent.click(screen.getByRole("button", { name: "Edit" }));

      expect(await screen.findByText("This dashboard is a draft.")).toBeInTheDocument();
      const publishBtn = screen.getByRole("button", { name: "Publish" });
      expect(publishBtn).toBeDisabled();
      expect(publishBtn).toHaveAttribute("title", "Add at least one widget to publish");
      // pointer-events must stay hoverable or the title tooltip above can never render
      expect(getComputedStyle(publishBtn).pointerEvents).not.toBe("none");

      expect(screen.queryByRole("button", { name: /restore/i })).not.toBeInTheDocument();
      expect(screen.queryByText(/version history/i)).not.toBeInTheDocument();
    });

    it("shows the neutral hint with a disabled Publish and no Discard for a clean published dashboard, and no banner in view mode", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(makeDashboard({ published: true, has_unpublished_changes: false })),
        ),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.getByText("Published")).toBeInTheDocument();
      expect(screen.queryByText("Unpublished changes")).not.toBeInTheDocument();
      expect(screen.queryByText(/Drag a tile by its header/)).not.toBeInTheDocument();

      await userEvent.click(screen.getByRole("button", { name: "Edit" }));

      expect(await screen.findByText(/Changes are saved as a draft\./)).toBeInTheDocument();
      expect(screen.queryByText(/Drag a tile by its header/)).not.toBeInTheDocument();
      const publishBtn = screen.getByRole("button", { name: "Publish changes" });
      expect(publishBtn).toBeDisabled();
      expect(publishBtn).toHaveAttribute("title", "No changes to publish");
      expect(screen.queryByRole("button", { name: "Discard changes" })).not.toBeInTheDocument();
      expect(screen.queryByText("You're editing a draft.")).not.toBeInTheDocument();
    });

    it("shows the drag hint on desktop when the dashboard has widgets", async () => {
      mockViewport(true);
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(makeDashboard({ published: true, has_unpublished_changes: false })),
        ),
      );
      renderDashboard();
      await screen.findByText("Sales Dashboard");
      await userEvent.click(screen.getByRole("button", { name: "Edit" }));
      expect(await screen.findByText(/Drag a tile by its header/)).toBeInTheDocument();
    });

    it("drops the drag hint when the dashboard has no widgets, even on desktop", async () => {
      mockViewport(true);
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(makeDashboard({ published: true, widgets: [] })),
        ),
      );
      renderDashboard();
      await screen.findByText("Sales Dashboard");
      await userEvent.click(screen.getByRole("button", { name: "Edit" }));
      expect(await screen.findByText(/Changes are saved as a draft\./)).toBeInTheDocument();
      expect(screen.queryByText(/Drag a tile by its header/)).not.toBeInTheDocument();
    });

    it("shows the pending banner + View-mode hint, and the publish-changes confirm carries the computed change summary", async () => {
      let publishCalled = false;
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(
            makeDashboard({
              published: true,
              has_unpublished_changes: true,
              pending_change_summary: { added: 3, removed: 1, moved: 2, resized: 1, settings_changed: 0 },
            }),
          ),
        ),
        http.post(`${BASE}/api/dashboards/sales/publish`, () => {
          publishCalled = true;
          return HttpResponse.json(makeDashboard({ published: true, has_unpublished_changes: false }));
        }),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      expect(screen.getByText("Published")).toBeInTheDocument();
      expect(screen.getByText("Unpublished changes")).toBeInTheDocument();
      expect(
        await screen.findByText(
          "You're viewing the published version. Switch to Edit to see your draft.",
        ),
      ).toBeInTheDocument();

      await userEvent.click(screen.getByRole("button", { name: "Edit" }));

      expect(await screen.findByText("You're editing a draft.")).toBeInTheDocument();
      await userEvent.click(screen.getByRole("button", { name: "Publish changes" }));

      const dialog = await screen.findByRole("dialog");
      expect(within(dialog).getByText("Publish your changes?")).toBeInTheDocument();
      expect(
        within(dialog).getByText("3 tiles added · 1 removed · 2 moved · 1 resized"),
      ).toBeInTheDocument();

      await userEvent.click(within(dialog).getByRole("button", { name: "Publish changes" }));

      await waitFor(() => expect(publishCalled).toBe(true));
      await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    });

    it("shows Discard changes only when pending, behind a destructive confirm", async () => {
      let discardCalled = false;
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(makeDashboard({ published: true, has_unpublished_changes: true })),
        ),
        http.post(`${BASE}/api/dashboards/sales/discard`, () => {
          discardCalled = true;
          return HttpResponse.json(makeDashboard({ published: true, has_unpublished_changes: false }));
        }),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");
      await userEvent.click(screen.getByRole("button", { name: "Edit" }));

      await userEvent.click(await screen.findByRole("button", { name: "Discard changes" }));
      const dialog = await screen.findByRole("dialog");
      expect(within(dialog).getByText("Discard your unpublished changes?")).toBeInTheDocument();

      await userEvent.click(within(dialog).getByRole("button", { name: "Discard changes" }));
      await waitFor(() => expect(discardCalled).toBe(true));
    });

    it("disables Share and Embed while never-published, with a 'Publish this dashboard first' hint", async () => {
      server.use(
        http.get(`${BASE}/api/dashboards/sales`, () =>
          HttpResponse.json(makeDashboard({ published: false, has_unpublished_changes: false })),
        ),
      );

      renderDashboard();
      await screen.findByText("Sales Dashboard");

      const shareBtn = screen.getByRole("button", { name: "Share" });
      const embedBtn = screen.getByRole("button", { name: /public link/i });
      expect(shareBtn).toBeDisabled();
      expect(shareBtn).toHaveAttribute("title", "Publish this dashboard first");
      expect(embedBtn).toBeDisabled();
      expect(embedBtn).toHaveAttribute("title", "Publish this dashboard first");
      expect(getComputedStyle(shareBtn).pointerEvents).not.toBe("none");
      expect(getComputedStyle(embedBtn).pointerEvents).not.toBe("none");
    });
  });
});

describe("DashboardPage Explain", () => {
  const vizWidget = (): Widget => ({ ...makeWidget(10, "Revenue"), visualization_id: 5, text: null });

  function useSession(flag: boolean) {
    server.use(
      http.get(`${BASE}/api/session`, () =>
        HttpResponse.json({
          user: { id: 1, name: "T", email: "t@example.com", groups: [1], org_id: 1, is_invitation_pending: false, disabled_at: null, profile_image_url: null, details: {}, created_at: "", updated_at: "" },
          org: { id: 1, name: "O", slug: "o", settings: {} },
          permissions: ["admin"],
          is_admin: true,
          client_config: { ai_dashboard_narratives: flag },
        }),
      ),
    );
  }

  const narrative = {
    id: 1,
    dashboard_id: 1,
    created_at: new Date().toISOString(),
    source: "manual",
    summary: "Revenue is up.",
    body: null,
    highlights: [],
    confidence: "high",
    model: "m",
  };

  function useDashboard() {
    server.use(
      http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard({ widgets: [vizWidget()] }))),
      http.get(`${BASE}/api/dashboards/1/narrative`, () => HttpResponse.json({}, { status: 404 })),
    );
  }

  it("hides Explain when the flag is off", async () => {
    useSession(false);
    useDashboard();
    renderDashboard();
    await screen.findByText("Sales Dashboard");
    expect(screen.queryByRole("button", { name: "Explain" })).toBeNull();
  });

  it("hides Explain in edit mode and when there is no non-text widget", async () => {
    useSession(true);
    useDashboard();
    renderDashboard();
    expect(await screen.findByRole("button", { name: "Explain" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Edit" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Explain" })).toBeNull());
  });

  it("does not offer Explain for a monitor-only dashboard", async () => {
    useSession(true);
    const w = { ...makeWidget(11, "Monitor"), visualization_id: null, text: null, options: { monitors: [1] }, monitors: [] };
    server.use(
      http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard({ widgets: [w] }))),
      http.get(`${BASE}/api/dashboards/1/narrative`, () => HttpResponse.json({}, { status: 404 })),
    );
    renderDashboard();
    await screen.findByText("Sales Dashboard");
    expect(screen.queryByRole("button", { name: "Explain" })).toBeNull();
  });

  it("does not offer Explain for a text-only dashboard", async () => {
    useSession(true);
    server.use(
      http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard())),
      http.get(`${BASE}/api/dashboards/1/narrative`, () => HttpResponse.json({}, { status: 404 })),
    );
    renderDashboard();
    await screen.findByText("Sales Dashboard");
    expect(screen.queryByRole("button", { name: "Explain" })).toBeNull();
  });

  it("Explain POSTs and shows the expanded panel", async () => {
    useSession(true);
    useDashboard();
    server.use(
      http.post(`${BASE}/api/dashboards/1/narrative`, () =>
        HttpResponse.json({ status: "ok", narrative }),
      ),
    );
    renderDashboard();
    await userEvent.click(await screen.findByRole("button", { name: "Explain" }));
    expect(await screen.findByText("Revenue is up.")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "AI summary" })).toBeInTheDocument();
  });

  it("shows a stored narrative collapsed on load", async () => {
    useSession(true);
    server.use(
      http.get(`${BASE}/api/dashboards/sales`, () => HttpResponse.json(makeDashboard({ widgets: [vizWidget()] }))),
      http.get(`${BASE}/api/dashboards/1/narrative`, () => HttpResponse.json({ narrative })),
    );
    renderDashboard();
    const toggle = await screen.findByRole("button", { name: /AI summary/ });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
  });

  it("renders a tagged failure message", async () => {
    useSession(true);
    useDashboard();
    server.use(
      http.post(`${BASE}/api/dashboards/1/narrative`, () =>
        HttpResponse.json({ status: "rate_limited", message: "Slow down.", retry_after_secs: 5 }),
      ),
    );
    renderDashboard();
    await userEvent.click(await screen.findByRole("button", { name: "Explain" }));
    expect((await screen.findAllByText(/Slow down\. \(try again in 5s\)/)).length).toBeGreaterThan(0);
  });
});
