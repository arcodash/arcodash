import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse, delay } from "msw";
import { server } from "./msw-server";
import GroupsPage from "@/pages/GroupsPage";

const BASE = "http://localhost:8080";

const GROUPS = [
  { id: 1, name: "Admin", type: "builtin" },
  { id: 2, name: "Default", type: "builtin" },
  { id: 3, name: "Engineers", type: "regular" },
];

const ROLES = [
  { id: 1, name: "Admin", permissions: ["admin"] },
  { id: 2, name: "Editor", permissions: ["create_query"] },
  { id: 3, name: "Viewer", permissions: ["view_query"] },
];

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <GroupsPage />
    </QueryClientProvider>,
  );
}

/** Default handlers for everything GroupsPage/ManageGroupDialog fetch; use `overrides` rather than
 * registering the same path twice (MSW priority across duplicates isn't guaranteed). */
function baseHandlers(overrides: {
  members?: ReturnType<typeof http.get>;
  roles?: ReturnType<typeof http.get>;
  dataSources?: ReturnType<typeof http.get>;
  allDataSources?: ReturnType<typeof http.get>;
} = {}) {
  return [
    http.get(`${BASE}/api/groups`, () => HttpResponse.json(GROUPS)),
    http.get(`${BASE}/api/roles`, () => HttpResponse.json(ROLES)),
    overrides.members ?? http.get(`${BASE}/api/groups/:id/members`, () => HttpResponse.json([])),
    overrides.dataSources ??
      http.get(`${BASE}/api/groups/:id/data_sources`, () =>
        HttpResponse.json({ restricted: false, entries: [] }),
      ),
    overrides.allDataSources ??
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({ data_sources: [], can_create: true }),
      ),
    http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    overrides.roles ?? http.get(`${BASE}/api/groups/:id/roles`, () => HttpResponse.json([])),
  ];
}

describe("GroupsPage", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("renders the group list", async () => {
    server.use(...baseHandlers());

    renderPage();

    expect(await screen.findByText("Admin")).toBeInTheDocument();
    expect(screen.getByText("Default")).toBeInTheDocument();
    expect(screen.getByText("Engineers")).toBeInTheDocument();
  });

  it("links to group data source access docs from the tab's helper text", async () => {
    server.use(...baseHandlers());

    renderPage();

    await screen.findByText("Admin");
    expect(
      screen.getByRole("link", { name: /learn more about group data source access/i }),
    ).toHaveAttribute(
      "href",
      "https://www.arcodash.com/docs/administration/groups-data-source-access",
    );
  });

  it("search filters the group list by name", async () => {
    server.use(...baseHandlers());

    renderPage();
    await screen.findByText("Engineers");

    await userEvent.type(screen.getByLabelText("Search groups"), "eng");

    expect(screen.getByText("Engineers")).toBeInTheDocument();
    expect(screen.queryByText("Admin")).not.toBeInTheDocument();
    expect(screen.queryByText("Default")).not.toBeInTheDocument();
  });

  it("shows a Restricted badge only for a group with per-data-source restriction", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, ({ params }) =>
          HttpResponse.json({ restricted: params.id === "3", entries: [] }),
        ),
      }),
    );

    renderPage();
    await screen.findByText("Engineers");

    const engineersRow = screen.getByText("Engineers").closest("tr")!;
    expect(await within(engineersRow).findByText("Restricted")).toBeInTheDocument();

    const adminRow = screen.getByText("Admin").closest("tr")!;
    expect(within(adminRow).queryByText("Restricted")).not.toBeInTheDocument();
  });

  it("warns on a group with no role assigned, same treatment as the Roles tab", async () => {
    server.use(...baseHandlers());

    renderPage();
    await screen.findByText("Engineers");

    const engineersRow = screen.getByText("Engineers").closest("tr")!;
    expect(
      await within(engineersRow).findByLabelText(
        "No role assigned — members of this group get no permissions.",
      ),
    ).toBeInTheDocument();
  });

  it("shows role chips in the Roles column", async () => {
    server.use(
      ...baseHandlers({
        roles: http.get(`${BASE}/api/groups/:id/roles`, ({ params }) =>
          HttpResponse.json(params.id === "1" ? [ROLES[0]] : []),
        ),
      }),
    );

    renderPage();
    await screen.findByText("Engineers");

    expect(await screen.findByText("Admin", { selector: "span" })).toBeInTheDocument();
  });

  it("submits the create group dialog", async () => {
    let postedBody: unknown = null;

    server.use(
      ...baseHandlers(),
      http.get(`${BASE}/api/groups`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/groups`, async ({ request }) => {
        postedBody = await request.json();
        return HttpResponse.json({ id: 99, name: "New Group", type: "regular" });
      }),
    );

    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "+ New group" }));
    const input = await screen.findByPlaceholderText("Group name");
    await userEvent.type(input, "New Group");
    await userEvent.click(screen.getByRole("button", { name: /^create$/i }));

    await waitFor(() => {
      expect(postedBody).toEqual({ name: "New Group" });
    });
  });

  it("shows an inline error and keeps the dialog open with the typed name when creating a group fails", async () => {
    server.use(
      ...baseHandlers(),
      http.get(`${BASE}/api/groups`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/groups`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: "+ New group" }));
    const input = await screen.findByPlaceholderText("Group name");
    await userEvent.type(input, "New Group");
    await userEvent.click(screen.getByRole("button", { name: /^create$/i }));

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("Group name")).toHaveValue("New Group");
  });

  it("shows Delete only for non-builtin groups", async () => {
    server.use(...baseHandlers());

    renderPage();
    await screen.findByText("Engineers");

    const deleteButtons = screen.getAllByRole("button", { name: "Delete" });
    expect(deleteButtons).toHaveLength(1);
  });

  it("delete uses a real confirm Dialog, not window.confirm", async () => {
    let deleteCalls = 0;
    server.use(
      ...baseHandlers(),
      http.delete(`${BASE}/api/groups/3`, () => {
        deleteCalls += 1;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage();
    await screen.findByText("Engineers");

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(await screen.findByText('Delete group "Engineers"?')).toBeInTheDocument();
    expect(deleteCalls).toBe(0);

    const confirmButtons = screen.getAllByRole("button", { name: "Delete" });
    await userEvent.click(confirmButtons[confirmButtons.length - 1]);

    await waitFor(() => expect(deleteCalls).toBe(1));
  });

  it("detaches a role via the Roles panel", async () => {
    let rolesForGroup: typeof ROLES = [ROLES[1]];
    server.use(
      ...baseHandlers({
        roles: http.get(`${BASE}/api/groups/:id/roles`, () => HttpResponse.json(rolesForGroup)),
      }),
      http.delete(`${BASE}/api/groups/3/roles/2`, () => {
        rolesForGroup = [];
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );
    await userEvent.click(await screen.findByLabelText("Detach Editor"));

    await waitFor(() => expect(screen.queryByLabelText("Detach Editor")).not.toBeInTheDocument());
  });

  it("Manage dialog's Roles panel attaches a role via POST /api/groups/:id/roles and shows the chip", async () => {
    let postedBody: unknown = null;
    let rolesForGroup: typeof ROLES = [];

    server.use(
      ...baseHandlers({
        roles: http.get(`${BASE}/api/groups/:id/roles`, () => HttpResponse.json(rolesForGroup)),
      }),
      http.post(`${BASE}/api/groups/3/roles`, async ({ request }) => {
        postedBody = await request.json();
        rolesForGroup = [ROLES[1]];
        return HttpResponse.json({ ok: true });
      }),
    );

    renderPage();
    await screen.findByText("Engineers");

    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );
    expect(await screen.findByText("Manage Engineers")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Attach role…" }));
    await userEvent.click(await screen.findByLabelText("Editor"));

    await waitFor(() => {
      expect(postedBody).toEqual({ role_id: 2 });
    });

    expect(await screen.findByLabelText("Detach Editor")).toBeInTheDocument();
  });

  it("Manage dialog lists members with name + email and a Remove control", async () => {
    server.use(
      ...baseHandlers({
        members: http.get(`${BASE}/api/groups/:id/members`, () =>
          HttpResponse.json([{ id: 5, name: "Jane Smith", email: "jane@example.com" }]),
        ),
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    expect(await screen.findByText("Jane Smith (jane@example.com)")).toBeInTheDocument();
    expect(screen.getByLabelText("Remove Jane Smith")).toBeInTheDocument();
  });

  it("Data sources toggle switches the helper text between off and on copy", async () => {
    let restricted = false;
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({ restricted, entries: [] }),
        ),
      }),
      http.put(`${BASE}/api/groups/3/data_source_access`, async ({ request }) => {
        const body = (await request.json()) as { restricted: boolean };
        restricted = body.restricted;
        return HttpResponse.json({ ok: true, restricted });
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    expect(
      await screen.findByText("This group can see and query all data sources."),
    ).toBeInTheDocument();

    await userEvent.click(screen.getByRole("switch"));

    expect(
      await screen.findByText("This group can see and query only the data sources listed below."),
    ).toBeInTheDocument();
  });

  it("shows the empty-list row when restricted with zero entries", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({ restricted: true, entries: [] }),
        ),
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    expect(
      await screen.findByText(
        "Restricted, but the list is empty — this group can't see any data source until you add one below.",
      ),
    ).toBeInTheDocument();
  });

  it("shows the manage-access warning when restricted and a linked role grants manage_data_source", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({
            restricted: true,
            entries: [{ id: 1, data_source_id: 10, group_id: 3, name: "Warehouse" }],
          }),
        ),
        roles: http.get(`${BASE}/api/groups/:id/roles`, () =>
          HttpResponse.json([{ id: 5, name: "DS Manager", permissions: ["manage_data_source"] }]),
        ),
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    expect(await screen.findByText("Warehouse")).toBeInTheDocument();
    const heading = await screen.findByText(
      "This group can manage data sources — but only the ones you allow.",
    );
    expect(heading).toBeInTheDocument();
    expect(
      screen.getByText(/Restricted is on and an attached role grants Manage data sources/),
    ).toBeInTheDocument();

    const warning = heading.closest("div")!.parentElement!;
    expect(warning).toHaveAttribute("aria-live", "polite");
    expect(warning.querySelector("svg")).toBeInTheDocument();

    const dialog = screen.getByRole("dialog");
    expect(screen.getByRole("switch")).not.toBeDisabled();
    expect(within(dialog).getByRole("button", { name: "Add a data source…" })).not.toBeDisabled();
    for (const closeButton of within(dialog).getAllByRole("button", { name: "Close" })) {
      expect(closeButton).not.toBeDisabled();
    }
  });

  it("hides the manage-access warning when restricted but no linked role grants manage_data_source", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({
            restricted: true,
            entries: [{ id: 1, data_source_id: 10, group_id: 3, name: "Warehouse" }],
          }),
        ),
        roles: http.get(`${BASE}/api/groups/:id/roles`, () =>
          HttpResponse.json([{ id: 3, name: "Viewer", permissions: ["view_query"] }]),
        ),
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    expect(await screen.findByText("Warehouse")).toBeInTheDocument();
    expect(
      screen.queryByText("This group can manage data sources — but only the ones you allow."),
    ).not.toBeInTheDocument();
  });

  it("hides the manage-access warning when unrestricted, even with a manage_data_source role", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({ restricted: false, entries: [] }),
        ),
        roles: http.get(`${BASE}/api/groups/:id/roles`, () =>
          HttpResponse.json([{ id: 5, name: "DS Manager", permissions: ["manage_data_source"] }]),
        ),
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    expect(
      await screen.findByText("This group can see and query all data sources."),
    ).toBeInTheDocument();
    expect(
      screen.queryByText("This group can manage data sources — but only the ones you allow."),
    ).not.toBeInTheDocument();
  });

  it("removing an allowlist entry calls DELETE /api/groups/:id/data_sources/:dsId", async () => {
    let removedId: number | null = null;
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({
            restricted: true,
            entries: [{ id: 1, data_source_id: 10, group_id: 3, name: "Warehouse" }],
          }),
        ),
      }),
      http.delete(`${BASE}/api/groups/3/data_sources/10`, () => {
        removedId = 10;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );
    await screen.findByText("Warehouse");

    await userEvent.click(screen.getByLabelText("Remove Warehouse"));

    await waitFor(() => expect(removedId).toBe(10));
  });

  it("adding a data source via the picker posts { data_source_id } to the group's allowlist", async () => {
    let postedBody: unknown = null;
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({ restricted: true, entries: [] }),
        ),
        allDataSources: http.get(`${BASE}/api/data_sources`, () =>
          HttpResponse.json({
            data_sources: [{ id: 10, name: "Warehouse", type: "pg" }],
            can_create: true,
          }),
        ),
      }),
      http.post(`${BASE}/api/groups/3/data_sources`, async ({ request }) => {
        postedBody = await request.json();
        return HttpResponse.json({ ok: true });
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    await userEvent.click(await screen.findByRole("button", { name: "Add a data source…" }));
    await userEvent.click(await screen.findByText("Warehouse"));

    await waitFor(() => {
      expect(postedBody).toEqual({ data_source_id: 10 });
    });
  });

  it("shows an inline error in the Manage dialog when removing a member fails silently otherwise", async () => {
    server.use(
      ...baseHandlers({
        members: http.get(`${BASE}/api/groups/:id/members`, () =>
          HttpResponse.json([{ id: 5, name: "Jane Smith", email: "jane@example.com" }]),
        ),
      }),
      http.delete(`${BASE}/api/groups/3/members/5`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    await screen.findByText("Jane Smith (jane@example.com)");
    await userEvent.click(screen.getByLabelText("Remove Jane Smith"));

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
    expect(screen.getByText("Jane Smith (jane@example.com)")).toBeInTheDocument();
  });

  it("shows an inline error in the Manage dialog when toggling Restricted fails", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({ restricted: false, entries: [] }),
        ),
      }),
      http.put(`${BASE}/api/groups/3/data_source_access`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    const toggle = await screen.findByLabelText("Restricted");
    await userEvent.click(toggle);

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });

  it("the restricted badge shows an unknown state while its query is in flight, never 'unrestricted'", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () => new Promise(() => {})),
      }),
    );

    renderPage();
    const engineersRow = (await screen.findByText("Engineers")).closest("tr")!;

    expect(
      await within(engineersRow).findByTitle("Checking data-source access…"),
    ).toBeInTheDocument();
    expect(within(engineersRow).queryByText("Restricted")).not.toBeInTheDocument();
  });

  it("a failed group-data-sources query does not render the group as unrestricted", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({ message: "boom" }, { status: 500 }),
        ),
      }),
    );

    renderPage();
    const engineersRow = (await screen.findByText("Engineers")).closest("tr")!;

    expect(
      await within(engineersRow).findByTitle("Checking data-source access…"),
    ).toBeInTheDocument();
    expect(within(engineersRow).queryByText("Restricted")).not.toBeInTheDocument();
  });

  it("no-role warning is not shown while the roles query is loading", async () => {
    server.use(
      ...baseHandlers({
        roles: http.get(`${BASE}/api/groups/:id/roles`, () => new Promise(() => {})),
      }),
    );

    renderPage();
    const engineersRow = (await screen.findByText("Engineers")).closest("tr")!;

    expect(
      within(engineersRow).queryByLabelText("No role assigned — members of this group get no permissions."),
    ).not.toBeInTheDocument();
  });

  it("member count renders a placeholder, not 0, while loading", async () => {
    server.use(
      ...baseHandlers({
        members: http.get(`${BASE}/api/groups/:id/members`, () => new Promise(() => {})),
      }),
    );

    renderPage();
    const engineersRow = (await screen.findByText("Engineers")).closest("tr")!;
    const membersCell = within(engineersRow).getAllByRole("cell")[3];

    expect(within(membersCell).getByText("—")).toBeInTheDocument();
    expect(within(membersCell).queryByText("0")).not.toBeInTheDocument();
  });

  it("a failed data-source toggle reports inside the data sources section, not next to Members", async () => {
    server.use(
      ...baseHandlers({
        dataSources: http.get(`${BASE}/api/groups/:id/data_sources`, () =>
          HttpResponse.json({ restricted: false, entries: [] }),
        ),
      }),
      http.put(`${BASE}/api/groups/3/data_source_access`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );

    const dialog = screen.getByRole("dialog");
    const dataSourcesHeading = await within(dialog).findByText("Data sources");
    const dataSourcesSection = dataSourcesHeading.closest("div")!;
    const membersHeading = within(dialog).getByRole("heading", { name: "Members" });
    const membersSection = membersHeading.closest("div")!;

    await userEvent.click(screen.getByLabelText("Restricted"));

    expect(await within(dataSourcesSection).findByText("database unavailable")).toBeInTheDocument();
    expect(within(membersSection).queryByText("database unavailable")).not.toBeInTheDocument();
  });

  it("a second role toggle while the first is in flight sends no second request", async () => {
    let requestCount = 0;
    server.use(
      ...baseHandlers(),
      http.post(`${BASE}/api/groups/3/roles`, async () => {
        requestCount += 1;
        await delay(50);
        return HttpResponse.json({ ok: true });
      }),
    );

    renderPage();
    await screen.findByText("Engineers");
    await userEvent.click(
      within(screen.getByText("Engineers").closest("tr")!).getByRole("button", { name: "Manage" }),
    );
    await userEvent.click(screen.getByRole("button", { name: "Attach role…" }));

    const editorCheckbox = await screen.findByLabelText("Editor");
    await userEvent.click(editorCheckbox);
    expect(editorCheckbox).toBeDisabled();

    // Disabled while in flight, so a second click is a no-op; use fireEvent since userEvent refuses
    // disabled controls.
    editorCheckbox.click();

    await waitFor(() => expect(requestCount).toBe(1));
    await new Promise((r) => setTimeout(r, 60));
    expect(requestCount).toBe(1);
  });
});
