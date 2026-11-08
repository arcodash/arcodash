import { describe, it, expect } from "vitest";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { ShareDialog } from "@/components/sharing/ShareDialog";

const BASE = "http://localhost:8080";

const GRANTS = [
  {
    id: 100,
    object_type: "query",
    object_id: 1,
    access_type: "viewer",
    grantor_id: 1,
    grantee_id: 2,
    grantee_group_id: null,
    created_at: new Date().toISOString(),
  },
  {
    id: 101,
    object_type: "query",
    object_id: 1,
    access_type: "editor",
    grantor_id: 1,
    grantee_id: null,
    grantee_group_id: 10,
    created_at: new Date().toISOString(),
  },
];

const USERS = [
  { id: 1, name: "Alice Owner", email: "alice@test.local" },
  { id: 2, name: "Bob Viewer", email: "bob@test.local" },
  { id: 3, name: "Carol Free", email: "carol@test.local" },
];
const GROUPS = [
  { id: 10, name: "Analytics", type: "regular", permissions: [], created_at: "", updated_at: "" },
];

function baseHandlers() {
  return [
    http.get(`${BASE}/api/queries/1/acl`, () => HttpResponse.json(GRANTS)),
    http.get(`${BASE}/api/users`, () => HttpResponse.json(USERS)),
    http.get(`${BASE}/api/groups`, () => HttpResponse.json(GROUPS)),
  ];
}

function renderDialog(props: Partial<Parameters<typeof ShareDialog>[0]> = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ShareDialog
        objectType="query"
        objectId={1}
        ownerId={1}
        visibility="restricted"
        open
        canManage
        onClose={() => {}}
        {...props}
      />
    </QueryClientProvider>,
  );
}

describe("ShareDialog", () => {
  it("renders the owner and existing user + group grants", async () => {
    server.use(...baseHandlers());
    renderDialog();
    expect(await screen.findByText("Bob Viewer")).toBeInTheDocument();
    expect(await screen.findByText("Analytics")).toBeInTheDocument();
    // Owner row resolves the ownerId to the user's name.
    expect(await screen.findByText("Alice Owner")).toBeInTheDocument();
  });

  it("changing a grant's role calls the grant endpoint with the new access_type", async () => {
    let body: Record<string, unknown> | null = null;
    server.use(
      ...baseHandlers(),
      http.post(`${BASE}/api/queries/1/acl`, async ({ request }) => {
        body = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({ ...GRANTS[0], access_type: body.access_type });
      }),
    );
    renderDialog();
    const roleSelect = await screen.findByRole("combobox", { name: /role for bob viewer/i });
    await userEvent.click(roleSelect);
    await userEvent.click(await screen.findByRole("option", { name: /^editor$/i }));
    await waitFor(() => expect(body).toEqual({ grantee_id: 2, access_type: "editor" }));
  });

  it("toggling visibility from Specific people to Everyone calls setVisibility", async () => {
    let body: Record<string, unknown> | null = null;
    server.use(
      ...baseHandlers(),
      http.put(`${BASE}/api/queries/1/visibility`, async ({ request }) => {
        body = (await request.json()) as Record<string, unknown>;
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderDialog({ visibility: "restricted" });
    await userEvent.click(await screen.findByRole("button", { name: /everyone/i }));
    await waitFor(() => expect(body).toEqual({ visibility: "workspace" }));
  });

  it("read-only mode (canManage=false) shows no add control or remove buttons", async () => {
    server.use(...baseHandlers());
    renderDialog({ canManage: false });
    await screen.findByText("Bob Viewer");
    expect(screen.queryByLabelText(/add a person or group/i)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /remove/i })).not.toBeInTheDocument();
    // Role renders as static text, not a Select.
    expect(screen.queryByRole("combobox", { name: /role for/i })).not.toBeInTheDocument();
  });

  it("does not fetch the ACL/users/groups until the dialog opens", async () => {
    let aclCalls = 0;
    server.use(
      http.get(`${BASE}/api/queries/1/acl`, () => {
        aclCalls++;
        return HttpResponse.json(GRANTS);
      }),
      http.get(`${BASE}/api/users`, () => HttpResponse.json(USERS)),
      http.get(`${BASE}/api/groups`, () => HttpResponse.json(GROUPS)),
    );
    const { rerender } = renderDialog({ open: false });
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20));
    });
    expect(aclCalls).toBe(0);
    expect(screen.queryByText("Bob Viewer")).not.toBeInTheDocument();

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    rerender(
      <QueryClientProvider client={qc}>
        <ShareDialog
          objectType="query"
          objectId={1}
          ownerId={1}
          visibility="restricted"
          open
          canManage
          onClose={() => {}}
        />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(aclCalls).toBeGreaterThan(0));
    expect(await screen.findByText("Bob Viewer")).toBeInTheDocument();
  });

  it("an owner-tier grant renders a static \"owner\" label, not an editable role dropdown", async () => {
    const grantsWithOwner = [
      ...GRANTS,
      {
        id: 102,
        object_type: "query",
        object_id: 1,
        access_type: "owner",
        grantor_id: 1,
        grantee_id: 3,
        grantee_group_id: null,
        created_at: new Date().toISOString(),
      },
    ];
    server.use(
      http.get(`${BASE}/api/queries/1/acl`, () => HttpResponse.json(grantsWithOwner)),
      http.get(`${BASE}/api/users`, () => HttpResponse.json(USERS)),
      http.get(`${BASE}/api/groups`, () => HttpResponse.json(GROUPS)),
    );
    renderDialog();
    const carolRow = (await screen.findByText("Carol Free")).closest("li")!;
    expect(within(carolRow).getByText("owner")).toBeInTheDocument();
    expect(within(carolRow).queryByRole("combobox")).not.toBeInTheDocument();
    // The owner-tier grant can still be revoked when canManage is true.
    expect(within(carolRow).getByRole("button", { name: /remove carol free/i })).toBeInTheDocument();

    // Unaffected: Bob's viewer grant still renders as an editable role dropdown.
    const bobRow = (await screen.findByText("Bob Viewer")).closest("li")!;
    expect(within(bobRow).getByRole("combobox")).toBeInTheDocument();
  });

  it("typing in the search box filters the add-grantee list to matching people/groups", async () => {
    const manyUsers = [
      { id: 1, name: "Alice Owner", email: "alice@test.local" },
      { id: 3, name: "Carol Free", email: "carol@test.local" },
      { id: 4, name: "Marco Polo", email: "marco@test.local" },
    ];
    const manyGroups = [
      { id: 20, name: "Marketing", type: "regular", permissions: [], created_at: "", updated_at: "" },
    ];
    server.use(
      http.get(`${BASE}/api/queries/1/acl`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/users`, () => HttpResponse.json(manyUsers)),
      http.get(`${BASE}/api/groups`, () => HttpResponse.json(manyGroups)),
    );
    renderDialog();
    await screen.findByPlaceholderText(/search people or groups/i);
    const addSelect = screen.getByLabelText(/add person or group/i);
    await userEvent.click(addSelect);
    expect(await screen.findByRole("option", { name: "Carol Free" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "Marco Polo" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /marketing/i })).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");

    await userEvent.type(screen.getByPlaceholderText(/search people or groups/i), "marco");
    await userEvent.click(addSelect);
    expect(await screen.findByRole("option", { name: "Marco Polo" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "Carol Free" })).not.toBeInTheDocument();
    expect(screen.queryByRole("option", { name: /marketing/i })).not.toBeInTheDocument();
  });

  it("shows an empty state when no person or group matches the search", async () => {
    server.use(...baseHandlers());
    renderDialog();
    await userEvent.type(
      await screen.findByPlaceholderText(/search people or groups/i),
      "zzz-no-such-name",
    );
    await userEvent.click(screen.getByLabelText(/add person or group/i));
    expect(await screen.findByText("No people or groups match")).toBeInTheDocument();
  });
});
