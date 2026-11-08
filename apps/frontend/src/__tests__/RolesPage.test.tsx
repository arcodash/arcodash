import { describe, it, expect } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import RolesPage from "@/pages/RolesPage";

const BASE = "http://localhost:8080";

const PERMISSIONS = [
  { key: "admin", label: "Administer workspace", category: "Admin" },
  { key: "view_users", label: "View users", category: "Admin" },
  { key: "create_query", label: "Create queries", category: "Queries" },
  { key: "view_query", label: "View queries", category: "Queries" },
];

const ROLES = [
  {
    id: 1,
    name: "Admin",
    type: "builtin",
    permissions: ["admin"],
    group_count: 1,
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
  },
  {
    id: 2,
    name: "Analyst",
    type: "custom",
    permissions: ["view_query"],
    group_count: 0,
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
  },
];

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <RolesPage />
    </QueryClientProvider>,
  );
}

describe("RolesPage", () => {
  it("renders the role list with a builtin badge and no Delete button for the builtin role", async () => {
    server.use(
      http.get(`${BASE}/api/roles`, () => HttpResponse.json(ROLES)),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
    );

    renderPage();

    expect(await screen.findByText("Admin")).toBeInTheDocument();
    expect(screen.getByText("Analyst")).toBeInTheDocument();
    expect(screen.getByText("Built-in")).toBeInTheDocument();
    expect(screen.getByText("Custom")).toBeInTheDocument();

    expect(screen.getAllByRole("button", { name: "Delete" })).toHaveLength(1);
    expect(screen.getByRole("button", { name: "View" })).toBeInTheDocument();
  });

  it("opens the editor when New role is clicked", async () => {
    server.use(
      http.get(`${BASE}/api/roles`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "+ New role" }));

    expect(await screen.findByText("New role")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("Role name")).toBeInTheDocument();
  });

  it("submitting a name and a checked permission creates the role and shows it in the list", async () => {
    let postedBody: unknown = null;
    let roles = [...ROLES];

    server.use(
      http.get(`${BASE}/api/roles`, () => HttpResponse.json(roles)),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
      http.post(`${BASE}/api/roles`, async ({ request }) => {
        postedBody = await request.json();
        const created = {
          id: 3,
          name: "Support",
          type: "custom",
          permissions: ["view_query"],
          group_count: 0,
          created_at: "2025-01-01T00:00:00Z",
          updated_at: "2025-01-01T00:00:00Z",
        };
        roles = [...roles, created];
        return HttpResponse.json(created);
      }),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "+ New role" }));

    await userEvent.type(await screen.findByPlaceholderText("Role name"), "Support");
    await userEvent.click(screen.getByLabelText("View queries"));
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(postedBody).toEqual({ name: "Support", permissions: ["view_query"] });
    });
    expect(await screen.findByText("Support")).toBeInTheDocument();
  });

  it("editing an existing custom role posts the update", async () => {
    let postedBody: unknown = null;

    server.use(
      http.get(`${BASE}/api/roles`, () => HttpResponse.json(ROLES)),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
      http.post(`${BASE}/api/roles/2`, async ({ request }) => {
        postedBody = await request.json();
        return HttpResponse.json({ ...ROLES[1], name: "Analyst Renamed" });
      }),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Edit" }));

    const nameInput = await screen.findByDisplayValue("Analyst");
    await userEvent.clear(nameInput);
    await userEvent.type(nameInput, "Analyst Renamed");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => expect(postedBody).not.toBeNull());
  });

  it("gates delete on group_count and shows the blocked-delete copy", async () => {
    server.use(
      http.get(`${BASE}/api/roles`, () => HttpResponse.json([{ ...ROLES[1], group_count: 2 }])),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "Delete" }));

    expect(await screen.findByText(/can't be deleted until it's detached/i)).toBeInTheDocument();
    const dialogDeleteButton = screen.getAllByRole("button", { name: "Delete" }).at(-1)!;
    expect(dialogDeleteButton).toBeDisabled();
  });

  it("builtin role editor opens read-only with disabled inputs and no Save button", async () => {
    server.use(
      http.get(`${BASE}/api/roles`, () => HttpResponse.json(ROLES)),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: "View" }));

    expect(await screen.findByText("Built-in role. Permissions are fixed and can't be changed.")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("Role name")).toBeDisabled();
    expect(screen.queryByRole("button", { name: /^save$/i })).not.toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "Close" }).length).toBeGreaterThanOrEqual(1);
  });

  it("renders the admin wildcard as 'All permissions' instead of a bare count of 1", async () => {
    server.use(
      http.get(`${BASE}/api/roles`, () => HttpResponse.json(ROLES)),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
    );

    renderPage();

    expect(await screen.findByText("All permissions")).toBeInTheDocument();
    // A plain count for non-wildcard roles.
    expect(screen.getByText("1 permission")).toBeInTheDocument();
  });

  it("flags a zero-permission role that's actually assigned to a group", async () => {
    server.use(
      http.get(`${BASE}/api/roles`, () =>
        HttpResponse.json([{ ...ROLES[1], name: "Empty In Use", permissions: [], group_count: 1 }]),
      ),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
    );

    renderPage();

    await screen.findByText("Empty In Use");
    expect(screen.getByLabelText("Assigned to a group but grants no permissions")).toBeInTheDocument();
  });

  it("does not flag a zero-permission role that isn't assigned to any group", async () => {
    server.use(
      http.get(`${BASE}/api/roles`, () =>
        HttpResponse.json([{ ...ROLES[1], name: "Empty Unused", permissions: [], group_count: 0 }]),
      ),
      http.get(`${BASE}/api/permissions`, () => HttpResponse.json(PERMISSIONS)),
    );

    renderPage();

    await screen.findByText("Empty Unused");
    expect(screen.queryByLabelText("Assigned to a group but grants no permissions")).not.toBeInTheDocument();
  });
});
