import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";
import { UsersPage, overriddenRestrictedGroups, type GroupScopePosture } from "@/pages/UsersPage";

const BASE = "http://localhost:8080";

// Default /api/session user id 1 is "self".
const SELF = {
  id: 1,
  org_id: 1,
  name: "Test User",
  email: "test@example.com",
  groups: [1],
  is_invitation_pending: false,
  disabled: false,
  created_at: "2025-01-01T00:00:00Z",
  updated_at: "2025-01-01T00:00:00Z",
};

const OTHER_ACTIVE = {
  id: 2,
  org_id: 1,
  name: "Alice Active",
  email: "alice@example.com",
  groups: [1],
  is_invitation_pending: false,
  disabled: false,
  created_at: "2025-01-01T00:00:00Z",
  updated_at: "2025-01-01T00:00:00Z",
};

const DISABLED = {
  id: 3,
  org_id: 1,
  name: "Bob Disabled",
  email: "bob@example.com",
  groups: [1],
  is_invitation_pending: false,
  disabled: true,
  created_at: "2025-01-01T00:00:00Z",
  updated_at: "2025-01-01T00:00:00Z",
};

const PENDING = {
  id: 5,
  org_id: 1,
  name: "Pat Pending",
  email: "pat@example.com",
  groups: [1],
  is_invitation_pending: true,
  disabled: false,
  created_at: "2025-01-01T00:00:00Z",
  updated_at: "2025-01-01T00:00:00Z",
};

const MULTI_GROUP_USER = {
  id: 6,
  org_id: 1,
  name: "Multi Group",
  email: "multi@example.com",
  groups: [2, 3],
  is_invitation_pending: false,
  disabled: false,
  created_at: "2025-01-01T00:00:00Z",
  updated_at: "2025-01-01T00:00:00Z",
};

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <UsersPage />
      </AuthProvider>
    </QueryClientProvider>,
  );
}

function mockUsers(users: unknown[]) {
  server.use(http.get(`${BASE}/api/users`, () => HttpResponse.json(users)));
}

/** Scope a query to the row containing `text`; action labels repeat across rows. */
function rowFor(text: string) {
  const cell = screen.getByText(text);
  return within(cell.closest("tr")!);
}

afterEach(() => {
  Object.assign(navigator, { clipboard: undefined });
});

describe("UsersPage", () => {
  it("does not render a Disable button on the signed-in user's own row", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    renderPage();

    await screen.findByText("Test User");
    await screen.findByText("Alice Active");

    expect(screen.getAllByRole("button", { name: "Disable" })).toHaveLength(1);
  });

  it("disable requires confirmation and only then issues DELETE", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    let deleteCalls = 0;
    server.use(
      http.delete(`${BASE}/api/users/2`, () => {
        deleteCalls += 1;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage();
    await screen.findByText("Alice Active");

    await userEvent.click(screen.getByRole("button", { name: "Disable" }));
    expect(await screen.findByText("Disable user?")).toBeInTheDocument();
    expect(deleteCalls).toBe(0);

    const disableButtons = screen.getAllByRole("button", { name: "Disable" });
    await userEvent.click(disableButtons[disableButtons.length - 1]);

    await waitFor(() => expect(deleteCalls).toBe(1));
  });

  it("failed disable shows an inline error in the dialog and keeps it open", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.delete(`${BASE}/api/users/2`, () =>
        HttpResponse.json({ message: "Not authorized" }, { status: 403 }),
      ),
    );

    renderPage();
    await screen.findByText("Alice Active");

    await userEvent.click(screen.getByRole("button", { name: "Disable" }));
    expect(await screen.findByText("Disable user?")).toBeInTheDocument();

    const disableButtons = screen.getAllByRole("button", { name: "Disable" });
    await userEvent.click(disableButtons[disableButtons.length - 1]);

    expect(await screen.findByText("Not authorized")).toBeInTheDocument();
    expect(screen.getByText("Disable user?")).toBeInTheDocument();
  });

  it("failed enable surfaces an inline error on the page", async () => {
    mockUsers([SELF, DISABLED]);
    server.use(
      http.post(`${BASE}/api/users/3`, () =>
        HttpResponse.json({ message: "Not authorized" }, { status: 403 }),
      ),
    );

    renderPage();
    await screen.findByText("Bob Disabled");

    await userEvent.click(rowFor("Bob Disabled").getByRole("button", { name: "Enable" }));

    expect(await screen.findByText("Not authorized")).toBeInTheDocument();
  });

  it("a disabled row shows only Enable, and enabling sends exactly {disabled:false}", async () => {
    mockUsers([SELF, DISABLED]);
    let postedBody: unknown = null;
    server.use(
      http.post(`${BASE}/api/users/3`, async ({ request }) => {
        postedBody = await request.json();
        return HttpResponse.json({ ...DISABLED, disabled: false });
      }),
    );

    renderPage();
    await screen.findByText("Bob Disabled");

    const bobRow = rowFor("Bob Disabled");
    expect(bobRow.queryByRole("button", { name: "Edit" })).not.toBeInTheDocument();
    expect(bobRow.queryByRole("button", { name: "Regenerate key" })).not.toBeInTheDocument();
    expect(bobRow.queryByRole("button", { name: "Disable" })).not.toBeInTheDocument();

    await userEvent.click(bobRow.getByRole("button", { name: "Enable" }));

    await waitFor(() => expect(postedBody).toEqual({ disabled: false }));
  });

  it("edit sends {name, groups} with no disabled key", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    let postedBody: unknown = null;
    server.use(
      http.get(`${BASE}/api/groups`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/groups/1/roles`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/users/2`, async ({ request }) => {
        postedBody = await request.json();
        return HttpResponse.json({ ...OTHER_ACTIVE, name: "Alice Renamed" });
      }),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Edit" }));

    const nameInput = await screen.findByDisplayValue("Alice Active");
    await userEvent.clear(nameInput);
    await userEvent.type(nameInput, "Alice Renamed");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(postedBody).toEqual({ name: "Alice Renamed", groups: [1] });
      expect((postedBody as Record<string, unknown>).disabled).toBeUndefined();
    });
  });

  it("the edit dialog's email renders as a full-opacity readonly value, not a disabled input", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.get(`${BASE}/api/groups`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/groups/1/roles`, () => HttpResponse.json([])),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Edit" }));

    const dialog = await screen.findByRole("dialog");
    const value = await within(dialog).findByText("alice@example.com");
    expect(value.tagName).toBe("P");
    expect(value).not.toBeDisabled();
    expect(value).not.toHaveClass("opacity-50");
  });

  it("regenerate goes confirm -> success -> Done, and reopening shows confirm again", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.post(`${BASE}/api/users/2/regenerate_api_key`, () =>
        HttpResponse.json({ api_key: "brand-new-key" }),
      ),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Regenerate key" }));

    expect(await screen.findByText("Regenerate API key?")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /^regenerate$/i }));

    expect(await screen.findByText("New API key")).toBeInTheDocument();
    expect(screen.getByDisplayValue("brand-new-key")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /^done$/i }));
    expect(screen.queryByText("New API key")).not.toBeInTheDocument();

    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Regenerate key" }));
    expect(await screen.findByText("Regenerate API key?")).toBeInTheDocument();
    expect(screen.queryByText("New API key")).not.toBeInTheDocument();
  });

  it("shows an inline error when regeneration fails, instead of failing silently", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.post(`${BASE}/api/users/2/regenerate_api_key`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Regenerate key" }));
    await userEvent.click(screen.getByRole("button", { name: /^regenerate$/i }));

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
    expect(screen.queryByText("New API key")).not.toBeInTheDocument();
  });

  it("copying the regenerated key shows Copied when the clipboard resolves", async () => {
    Object.assign(navigator, { clipboard: { writeText: vi.fn().mockResolvedValue(undefined) } });
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.post(`${BASE}/api/users/2/regenerate_api_key`, () =>
        HttpResponse.json({ api_key: "brand-new-key" }),
      ),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Regenerate key" }));
    await userEvent.click(screen.getByRole("button", { name: /^regenerate$/i }));
    await screen.findByDisplayValue("brand-new-key");

    await userEvent.click(screen.getByRole("button", { name: /^copy$/i }));
    expect(await screen.findByRole("button", { name: /^copied$/i })).toBeInTheDocument();
  });

  it("shows a manual-copy note and does not show Copied when the clipboard is unavailable", async () => {
    Object.assign(navigator, { clipboard: undefined });
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.post(`${BASE}/api/users/2/regenerate_api_key`, () =>
        HttpResponse.json({ api_key: "brand-new-key" }),
      ),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Regenerate key" }));
    await userEvent.click(screen.getByRole("button", { name: /^regenerate$/i }));
    await screen.findByDisplayValue("brand-new-key");

    await userEvent.click(screen.getByRole("button", { name: /^copy$/i }));

    expect(await screen.findByText(/select the key above/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^copied$/i })).not.toBeInTheDocument();
    expect(screen.getByDisplayValue("brand-new-key")).toBeInTheDocument();
  });

  it("shows a manual-copy note when writeText rejects", async () => {
    Object.assign(navigator, { clipboard: { writeText: vi.fn().mockRejectedValue(new Error("denied")) } });
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.post(`${BASE}/api/users/2/regenerate_api_key`, () =>
        HttpResponse.json({ api_key: "brand-new-key" }),
      ),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Regenerate key" }));
    await userEvent.click(screen.getByRole("button", { name: /^regenerate$/i }));
    await screen.findByDisplayValue("brand-new-key");

    await userEvent.click(screen.getByRole("button", { name: /^copy$/i }));

    expect(await screen.findByText(/select the key above/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^copied$/i })).not.toBeInTheDocument();
  });

  it("search filters by name or email, with an EmptyState on no match", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    renderPage();

    await screen.findByText("Alice Active");
    const search = screen.getByLabelText("Search users");

    await userEvent.type(search, "alice");
    expect(screen.getByText("Alice Active")).toBeInTheDocument();
    expect(screen.queryByText("Test User")).not.toBeInTheDocument();

    await userEvent.clear(search);
    await userEvent.type(search, "example.com");
    expect(screen.getByText("Alice Active")).toBeInTheDocument();
    expect(screen.getByText("Test User")).toBeInTheDocument();

    await userEvent.clear(search);
    await userEvent.type(search, "nobody-matches-this");
    expect(await screen.findByText("No users match your search.")).toBeInTheDocument();
  });

  it("hides the Email column below md, showing the email under the name instead", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    renderPage();

    await screen.findByText("Alice Active");

    const emailCells = screen.getAllByText("alice@example.com");
    const columnCell = emailCells.find((el) => el.tagName === "TD");
    expect(columnCell).toHaveClass("hidden", "md:table-cell");

    const mobileEmail = emailCells.find((el) => el !== columnCell);
    expect(mobileEmail).toHaveClass("md:hidden");
  });

  it("uses per-row pending state — enabling one row doesn't disable another row's button", async () => {
    const disabledOther = { ...DISABLED, id: 4, name: "Carol Disabled", email: "carol@example.com" };
    mockUsers([SELF, DISABLED, disabledOther]);
    // Held open so the pending state can't expire under CPU load.
    let releaseBob!: () => void;
    const bobGate = new Promise<void>((r) => (releaseBob = r));
    server.use(
      http.post(`${BASE}/api/users/3`, async () => {
        await bobGate;
        return HttpResponse.json({ ...DISABLED, disabled: false });
      }),
      http.post(`${BASE}/api/users/4`, () => HttpResponse.json({ ...disabledOther, disabled: false })),
    );

    renderPage();
    await screen.findByText("Bob Disabled");
    await screen.findByText("Carol Disabled");

    const enableButtons = screen.getAllByRole("button", { name: "Enable" });
    await userEvent.click(enableButtons[0]);

    expect(await screen.findByRole("button", { name: "Enabling…" })).toBeInTheDocument();
    const carolButton = screen.getByRole("button", { name: "Enable" });
    expect(carolButton).not.toBeDisabled();

    releaseBob();
    // Let the delayed request settle so its callback doesn't fire after unmount/teardown.
    await waitFor(() => expect(screen.queryByRole("button", { name: "Enabling…" })).not.toBeInTheDocument());
  });

  it("shows a Groups chip column mapping each user's group ids to names", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.get(`${BASE}/api/groups`, () =>
        HttpResponse.json([{ id: 1, name: "default", type: "builtin", permissions: [] }]),
      ),
    );

    renderPage();
    await screen.findByText("Alice Active");

    expect((await screen.findAllByText("default")).length).toBeGreaterThanOrEqual(1);
  });

  it("invite dialog opens with no group pre-selected, requires picking one, and sends exactly the picked ids", async () => {
    mockUsers([SELF]);
    let postedBody: unknown = null;
    server.use(
      http.get(`${BASE}/api/groups`, () =>
        HttpResponse.json([
          { id: 1, name: "default", type: "builtin", permissions: [] },
          { id: 2, name: "Engineers", type: "regular", permissions: [] },
          { id: 3, name: "viewers", type: "builtin", permissions: [] },
        ]),
      ),
      http.get(`${BASE}/api/groups/1/roles`, () =>
        HttpResponse.json([{ id: 10, name: "Editor", permissions: [] }]),
      ),
      http.get(`${BASE}/api/groups/2/roles`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/groups/3/roles`, () =>
        HttpResponse.json([{ id: 11, name: "Viewer", permissions: [] }]),
      ),
      http.post(`${BASE}/api/users`, async ({ request }) => {
        postedBody = await request.json();
        return HttpResponse.json({ ...SELF, id: 9, name: "New Person", email: "new@example.com" });
      }),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "+ Invite User" }));

    expect(screen.queryByLabelText(/Remove /)).not.toBeInTheDocument();
    expect(
      await screen.findByText("Pick at least one group — groups decide what this user can see and do."),
    ).toBeInTheDocument();

    await userEvent.type(screen.getByPlaceholderText("Jane Smith"), "New Person");
    await userEvent.type(screen.getByPlaceholderText("jane@example.com"), "new@example.com");

    const sendButton = screen.getByRole("button", { name: /send invitation/i });
    expect(sendButton).toBeDisabled();

    await userEvent.click(screen.getByRole("button", { name: "Add group…" }));

    const rows = (await screen.findAllByRole("checkbox")).map(
      (el) => el.closest("label")?.textContent,
    );
    expect(rows).toEqual(["viewers — Viewer", "default — Editor", "Engineers — No role assigned"]);

    await userEvent.click(screen.getByRole("checkbox", { name: /^viewers/ }));
    expect(sendButton).toBeEnabled();

    const removeButton = await screen.findByLabelText("Remove viewers");
    expect(removeButton.closest('[data-slot="badge"]')).toHaveTextContent("viewers — Viewer");

    await userEvent.click(sendButton);

    await waitFor(() => {
      expect(postedBody).toEqual({ name: "New Person", email: "new@example.com", groups: [3] });
    });
  });

  it("an invite with a malformed email cannot be submitted", async () => {
    mockUsers([SELF]);
    server.use(
      http.get(`${BASE}/api/groups`, () =>
        HttpResponse.json([{ id: 3, name: "viewers", type: "builtin", permissions: [] }]),
      ),
      http.get(`${BASE}/api/groups/3/roles`, () => HttpResponse.json([])),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "+ Invite User" }));

    await userEvent.type(screen.getByLabelText("Name"), "New Person");
    await userEvent.type(screen.getByLabelText("Email"), "not-an-email");
    await userEvent.click(screen.getByRole("button", { name: "Add group…" }));
    await userEvent.click(await screen.findByRole("checkbox", { name: /^viewers/ }));

    expect(await screen.findByText("Enter a valid email address.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /send invitation/i })).toBeDisabled();
  });

  it("a valid email enables the button", async () => {
    mockUsers([SELF]);
    server.use(
      http.get(`${BASE}/api/groups`, () =>
        HttpResponse.json([{ id: 3, name: "viewers", type: "builtin", permissions: [] }]),
      ),
      http.get(`${BASE}/api/groups/3/roles`, () => HttpResponse.json([])),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "+ Invite User" }));

    await userEvent.type(screen.getByLabelText("Name"), "New Person");
    await userEvent.type(screen.getByLabelText("Email"), "new@example.com");
    await userEvent.click(screen.getByRole("button", { name: "Add group…" }));
    await userEvent.click(await screen.findByRole("checkbox", { name: /^viewers/ }));

    expect(screen.queryByText("Enter a valid email address.")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /send invitation/i })).toBeEnabled();
  });

  it("invite dialog warns when picking a restricted group plus an unrestricted one, naming the restricted group", async () => {
    mockUsers([SELF]);
    server.use(
      http.get(`${BASE}/api/groups`, () =>
        HttpResponse.json([
          { id: 2, name: "Analysts", type: "regular", permissions: [] },
          { id: 3, name: "viewers", type: "builtin", permissions: [] },
        ]),
      ),
      http.get(`${BASE}/api/groups/2/roles`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/groups/3/roles`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/groups/2/data_sources`, () => HttpResponse.json({ restricted: true, entries: [] })),
      http.get(`${BASE}/api/groups/3/data_sources`, () => HttpResponse.json({ restricted: false, entries: [] })),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "+ Invite User" }));
    await userEvent.type(screen.getByPlaceholderText("Jane Smith"), "New Person");

    await userEvent.click(screen.getByRole("button", { name: "Add group…" }));
    await userEvent.click(await screen.findByRole("checkbox", { name: /^Analysts/ }));
    await userEvent.click(screen.getByRole("checkbox", { name: /^viewers/ }));

    expect(
      await screen.findByText(/Because New Person is also in an unrestricted group/),
    ).toBeInTheDocument();
    expect(screen.getByText(/on Analysts no longer applies/)).toBeInTheDocument();
  });

  it("edit-user dialog keeps the empty-groups hint and an enabled Save with zero groups", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.get(`${BASE}/api/groups`, () =>
        HttpResponse.json([{ id: 1, name: "default", type: "builtin", permissions: [] }]),
      ),
      http.get(`${BASE}/api/groups/1/roles`, () => HttpResponse.json([])),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Edit" }));

    await userEvent.click(screen.getByLabelText("Remove default"));

    expect(screen.getByText("No groups selected.")).toBeInTheDocument();
    expect(
      screen.queryByText("Pick at least one group — groups decide what this user can see and do."),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^save$/i })).toBeEnabled();
  });

  it("edit dialog shows read-only effective permissions grouped by category, with a Workspace admin pill when is_admin", async () => {
    mockUsers([SELF, OTHER_ACTIVE]);
    server.use(
      http.get(`${BASE}/api/groups`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/permissions`, () =>
        HttpResponse.json([
          { key: "admin", label: "Administer workspace", category: "Admin" },
          { key: "view_query", label: "View queries", category: "Queries" },
        ]),
      ),
      http.get(`${BASE}/api/groups/1/roles`, () =>
        HttpResponse.json([{ id: 1, name: "Admin Role", permissions: ["admin", "view_query"] }]),
      ),
      http.get(`${BASE}/api/users/2/effective_permissions`, () =>
        HttpResponse.json({ permissions: ["admin", "view_query"], is_admin: true }),
      ),
    );

    renderPage();
    await screen.findByText("Alice Active");
    await userEvent.click(rowFor("Alice Active").getByRole("button", { name: "Edit" }));

    expect(await screen.findByText("Effective permissions")).toBeInTheDocument();
    expect(
      screen.getByText(/Derived from this user's groups and roles/),
    ).toBeInTheDocument();
    expect(screen.getByText("Workspace admin")).toBeInTheDocument();
    expect(screen.getByText("Administer workspace")).toBeInTheDocument();
    expect(screen.getByText("View queries")).toBeInTheDocument();

    expect(screen.queryAllByRole("checkbox")).toHaveLength(0);
  });

  it("fetches its own ['users', {includeDisabled:true}] entry instead of reading another page's active-only ['users'] cache", async () => {
    // Simulates another page's picker having cached active-only users under ["users"]; staleTime
    // matches the real QueryClient, else the entry refetches on mount and masks the collision.
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 30_000 } } });
    qc.setQueryData(["users"], [SELF, OTHER_ACTIVE]);

    server.use(
      http.get(`${BASE}/api/users`, ({ request }) => {
        const includeDisabled = new URL(request.url).searchParams.get("include_disabled") === "true";
        return HttpResponse.json(includeDisabled ? [SELF, OTHER_ACTIVE, DISABLED] : [SELF, OTHER_ACTIVE]);
      }),
    );

    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <UsersPage />
        </AuthProvider>
      </QueryClientProvider>,
    );

    expect(await screen.findByText("Bob Disabled")).toBeInTheDocument();
    expect(rowFor("Bob Disabled").getByText("Disabled")).toBeInTheDocument();
  });

  it("a group id with no matching group renders a muted '—' fallback chip, not a raw id", async () => {
    mockUsers([SELF]);
    server.use(
      http.get(`${BASE}/api/groups`, () => HttpResponse.json([])),
    );

    renderPage();
    await screen.findByText("Test User");

    expect(await screen.findByText("—")).toBeInTheDocument();
    expect(screen.queryByText("#1")).not.toBeInTheDocument();
    expect(screen.queryByText("Unknown group")).not.toBeInTheDocument();
  });

  it("a pending-invitation row offers Edit, Resend invitation, and Revoke instead of Enable", async () => {
    mockUsers([SELF, PENDING]);
    renderPage();

    await screen.findByText("Pat Pending");
    expect(await screen.findByText("Invited")).toBeInTheDocument();
    const row = rowFor("Pat Pending");
    expect(row.getByRole("button", { name: "Edit" })).toBeInTheDocument();
    expect(row.getByRole("button", { name: "Resend invitation" })).toBeInTheDocument();
    expect(row.getByRole("button", { name: "Revoke" })).toBeInTheDocument();
    expect(row.queryByRole("button", { name: "Enable" })).not.toBeInTheDocument();
    expect(row.queryByRole("button", { name: "Regenerate key" })).not.toBeInTheDocument();
  });

  it("resend invitation posts to the resend endpoint and surfaces a failure inline", async () => {
    mockUsers([SELF, PENDING]);
    let resendCalls = 0;
    server.use(
      http.post(`${BASE}/api/users/5/resend_invitation`, () => {
        resendCalls += 1;
        return HttpResponse.json({ message: "SMTP not configured" }, { status: 500 });
      }),
    );

    renderPage();
    await screen.findByText("Pat Pending");
    await userEvent.click(rowFor("Pat Pending").getByRole("button", { name: "Resend invitation" }));

    await waitFor(() => expect(resendCalls).toBe(1));
    expect(await screen.findByText("SMTP not configured")).toBeInTheDocument();
  });

  it("a successful resend confirms in the row it was clicked in", async () => {
    const otherPending = { ...PENDING, id: 8, name: "Other Pending", email: "other@example.com" };
    mockUsers([SELF, PENDING, otherPending]);
    server.use(http.post(`${BASE}/api/users/5/resend_invitation`, () => new HttpResponse(null, { status: 204 })));

    renderPage();
    await screen.findByText("Pat Pending");
    await userEvent.click(rowFor("Pat Pending").getByRole("button", { name: "Resend invitation" }));

    expect(await rowFor("Pat Pending").findByText("Invitation resent.")).toBeInTheDocument();
    expect(rowFor("Other Pending").queryByText("Invitation resent.")).not.toBeInTheDocument();
  });

  it("a failed resend reports in that same row", async () => {
    const otherPending = { ...PENDING, id: 8, name: "Other Pending", email: "other@example.com" };
    mockUsers([SELF, PENDING, otherPending]);
    server.use(
      http.post(`${BASE}/api/users/5/resend_invitation`, () =>
        HttpResponse.json({ message: "SMTP not configured" }, { status: 500 }),
      ),
    );

    renderPage();
    await screen.findByText("Pat Pending");
    await userEvent.click(rowFor("Pat Pending").getByRole("button", { name: "Resend invitation" }));

    expect(await rowFor("Pat Pending").findByText("SMTP not configured")).toBeInTheDocument();
    expect(rowFor("Other Pending").queryByText("SMTP not configured")).not.toBeInTheDocument();
  });

  it("revoke requires confirmation and only then issues DELETE on the invitation", async () => {
    mockUsers([SELF, PENDING]);
    let deleteCalls = 0;
    server.use(
      http.delete(`${BASE}/api/users/5/invitation`, () => {
        deleteCalls += 1;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage();
    await screen.findByText("Pat Pending");
    await userEvent.click(rowFor("Pat Pending").getByRole("button", { name: "Revoke" }));

    expect(await screen.findByText("Revoke invitation?")).toBeInTheDocument();
    expect(deleteCalls).toBe(0);

    const revokeButtons = screen.getAllByRole("button", { name: "Revoke" });
    await userEvent.click(revokeButtons[revokeButtons.length - 1]);

    await waitFor(() => expect(deleteCalls).toBe(1));
  });

  it("failed revoke shows an inline error in the dialog and keeps it open", async () => {
    mockUsers([SELF, PENDING]);
    server.use(
      http.delete(`${BASE}/api/users/5/invitation`, () =>
        HttpResponse.json({ message: "Not authorized" }, { status: 403 }),
      ),
    );

    renderPage();
    await screen.findByText("Pat Pending");
    await userEvent.click(rowFor("Pat Pending").getByRole("button", { name: "Revoke" }));
    expect(await screen.findByText("Revoke invitation?")).toBeInTheDocument();

    const revokeButtons = screen.getAllByRole("button", { name: "Revoke" });
    await userEvent.click(revokeButtons[revokeButtons.length - 1]);

    expect(await screen.findByText("Not authorized")).toBeInTheDocument();
    expect(screen.getByText("Revoke invitation?")).toBeInTheDocument();
  });

  it("edit dialog's effective permissions recompute live from unsaved group chip edits", async () => {
    mockUsers([SELF, MULTI_GROUP_USER]);
    server.use(
      http.get(`${BASE}/api/groups`, () =>
        HttpResponse.json([
          { id: 2, name: "Engineering", type: "regular", permissions: [] },
          { id: 3, name: "Sales", type: "regular", permissions: [] },
        ]),
      ),
      http.get(`${BASE}/api/permissions`, () =>
        HttpResponse.json([
          { key: "view_query", label: "View queries", category: "Queries" },
          { key: "admin", label: "Administer workspace", category: "Admin" },
        ]),
      ),
      http.get(`${BASE}/api/groups/2/roles`, () =>
        HttpResponse.json([{ id: 10, name: "Eng Role", permissions: ["view_query"] }]),
      ),
      http.get(`${BASE}/api/groups/3/roles`, () =>
        HttpResponse.json([{ id: 11, name: "Sales Role", permissions: ["admin"] }]),
      ),
      http.get(`${BASE}/api/groups/2/data_sources`, () => HttpResponse.json({ restricted: false, entries: [] })),
      http.get(`${BASE}/api/groups/3/data_sources`, () => HttpResponse.json({ restricted: false, entries: [] })),
      http.get(`${BASE}/api/users/6/effective_permissions`, () =>
        HttpResponse.json({ permissions: [], is_admin: false }),
      ),
    );

    renderPage();
    await screen.findByText("Multi Group");
    await userEvent.click(rowFor("Multi Group").getByRole("button", { name: "Edit" }));

    await screen.findByText("Effective permissions");
    await screen.findByText("View queries");
    expect(screen.getByText("Administer workspace")).toBeInTheDocument();
    expect(screen.getByText("Workspace admin")).toBeInTheDocument();

    await userEvent.click(screen.getByLabelText("Remove Sales"));
    await waitFor(() => expect(screen.queryByText("Workspace admin")).not.toBeInTheDocument());
    expect(screen.queryByText("Administer workspace")).not.toBeInTheDocument();
    expect(screen.getByText("View queries")).toBeInTheDocument();

    await userEvent.click(screen.getByLabelText("Remove Engineering"));
    await waitFor(() =>
      expect(
        screen.getByText("No permissions yet. Add this user to a group to grant access."),
      ).toBeInTheDocument(),
    );
    expect(screen.queryByText("View queries")).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Add group…" }));
    await userEvent.click(await screen.findByRole("checkbox", { name: /^Sales/ }));
    await waitFor(() => expect(screen.getByText("Workspace admin")).toBeInTheDocument());
    expect(screen.getByText("Administer workspace")).toBeInTheDocument();
  });

  it("a 403 on the groups list renders a muted '—' badge instead of claiming 'Unknown group'", async () => {
    mockUsers([SELF]);
    server.use(
      http.get(`${BASE}/api/groups`, () => HttpResponse.json({ message: "Not authorized" }, { status: 403 })),
    );

    renderPage();
    await screen.findByText("Test User");

    const badge = await screen.findByTitle("Group names aren't visible without the view_groups permission.");
    expect(badge).toHaveTextContent("—");
    expect(screen.queryByText("Unknown group")).not.toBeInTheDocument();
  });

  it("adding a data-restricted user to an unrestricted group warns before save, naming the restricted group", async () => {
    const RESTRICTED_USER = {
      id: 7,
      org_id: 1,
      name: "Owen Pryce",
      email: "owen@example.com",
      groups: [2],
      is_invitation_pending: false,
      disabled: false,
      created_at: "2025-01-01T00:00:00Z",
      updated_at: "2025-01-01T00:00:00Z",
    };
    mockUsers([SELF, RESTRICTED_USER]);
    server.use(
      http.get(`${BASE}/api/groups`, () =>
        HttpResponse.json([
          { id: 2, name: "Analysts", type: "regular", permissions: [] },
          { id: 3, name: "viewers", type: "builtin", permissions: [] },
        ]),
      ),
      http.get(`${BASE}/api/groups/2/roles`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/groups/3/roles`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/groups/2/data_sources`, () =>
        HttpResponse.json({
          restricted: true,
          entries: [{ id: 1, data_source_id: 100, group_id: 2, name: "Prod DB" }],
        }),
      ),
      http.get(`${BASE}/api/groups/3/data_sources`, () => HttpResponse.json({ restricted: false, entries: [] })),
    );

    renderPage();
    await screen.findByText("Owen Pryce");
    await userEvent.click(rowFor("Owen Pryce").getByRole("button", { name: "Edit" }));

    await screen.findByRole("button", { name: /^save$/i });
    expect(screen.queryByText(/data source restriction/)).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Add group…" }));
    await userEvent.click(await screen.findByRole("checkbox", { name: /^viewers/ }));

    expect(
      await screen.findByText(/Because Owen Pryce is also in an unrestricted group/),
    ).toBeInTheDocument();
    expect(screen.getByText(/on Analysts no longer applies/)).toBeInTheDocument();

    expect(screen.getByRole("button", { name: /^save$/i })).toBeEnabled();
  });
});

describe("overriddenRestrictedGroups", () => {
  const restricted = (id: number, name: string): GroupScopePosture => ({ id, name, restricted: true });
  const unrestricted = (id: number, name: string): GroupScopePosture => ({ id, name, restricted: false });

  it("no warning when every selected group is restricted", () => {
    expect(overriddenRestrictedGroups([restricted(1, "Analysts"), restricted(2, "Finance")])).toEqual([]);
  });

  it("no warning when every selected group is unrestricted", () => {
    expect(overriddenRestrictedGroups([unrestricted(1, "viewers"), unrestricted(2, "default")])).toEqual([]);
  });

  it("names the restricted group when mixed with one unrestricted group", () => {
    const result = overriddenRestrictedGroups([restricted(1, "Analysts"), unrestricted(2, "viewers")]);
    expect(result).toEqual([restricted(1, "Analysts")]);
  });

  it("names every affected restricted group, not just one, when mixed with an unrestricted group", () => {
    const result = overriddenRestrictedGroups([
      restricted(1, "Analysts"),
      restricted(2, "Finance"),
      unrestricted(3, "viewers"),
    ]);
    expect(result.map((g) => g.name)).toEqual(["Analysts", "Finance"]);
  });

  it("no warning with a single restricted group and nothing else selected", () => {
    expect(overriddenRestrictedGroups([restricted(1, "Analysts")])).toEqual([]);
  });
});
