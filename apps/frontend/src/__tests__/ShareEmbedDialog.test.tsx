import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { ShareEmbedDialog } from "@/components/share/ShareEmbedDialog";
import type { Embed, Visibility } from "@/types/api";

const BASE = "http://localhost:8080";

function makeEmbed(overrides: Partial<Embed> = {}): Embed {
  return {
    id: 1,
    object_type: "visualization",
    object_id: 9,
    mode: "public",
    token: "tok-abc",
    public_url: "http://localhost:3000/public/embed/tok-abc",
    locked_params: {},
    expires_at: null,
    anon_authorized: true,
    revoked_at: null,
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function renderDialog(onClose = vi.fn(), visibility: Visibility = "public") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ShareEmbedDialog
        objectType="visualization"
        objectId={9}
        resourceTitle="Revenue by month"
        visibility={visibility}
        onClose={onClose}
      />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  Object.assign(navigator, {
    clipboard: { writeText: vi.fn().mockResolvedValue(undefined) },
  });
});

/** Default session is full-admin; override to one with create_embed but not share_public_anonymous
 * (sign-in-required links only). */
function noAnonymousPermissionSession() {
  server.use(
    http.get(`${BASE}/api/session`, () =>
      HttpResponse.json({
        user: { id: 1, name: "Test User", email: "test@example.com" },
        org: { id: 1, name: "Test Org", slug: "test-org" },
        permissions: ["create_embed"],
        is_admin: false,
      }),
    ),
  );
}

describe("ShareEmbedDialog", () => {
  it("titles itself Public link and states No public link when nothing has been created yet", async () => {
    server.use(http.get(`${BASE}/api/embeds`, () => HttpResponse.json([])));

    renderDialog();

    expect(await screen.findByText(/^Public link/)).toBeInTheDocument();
    expect(await screen.findByText(/No public link\./)).toBeInTheDocument();
    expect(screen.queryByText(/is private/i)).not.toBeInTheDocument();
  });

  it("links to the public links / embeds docs", async () => {
    server.use(http.get(`${BASE}/api/embeds`, () => HttpResponse.json([])));

    renderDialog();

    await screen.findByText(/^Public link/);
    expect(
      screen.getByRole("link", { name: /learn more about public links and embeds/i }),
    ).toHaveAttribute("href", "https://www.arcodash.com/docs/sharing-collaboration/public-links");
  });

  it("does not claim anonymous access is possible when the caller lacks share_public_anonymous", async () => {
    noAnonymousPermissionSession();
    server.use(http.get(`${BASE}/api/embeds`, () => HttpResponse.json([])));

    renderDialog();

    expect(
      await screen.findByText(/must first sign in to your workspace/i),
    ).toBeInTheDocument();
    expect(screen.queryByText(/without signing in/i)).not.toBeInTheDocument();
    expect(
      screen.queryByRole("checkbox", { name: /no sign-in/i }),
    ).not.toBeInTheDocument();

    expect(
      screen.getByRole("checkbox", {
        name: /accessible to anyone with the link who signs in to your workspace/i,
      }),
    ).toBeInTheDocument();
  });

  it("shows the no-public-link/amber-callout state when there are no active links, and the create button stays disabled until confirmed", async () => {
    server.use(http.get(`${BASE}/api/embeds`, () => HttpResponse.json([])));

    renderDialog();

    expect(await screen.findByText(/publicly accessible to anyone with the link/i)).toBeInTheDocument();
    const createBtn = screen.getByRole("button", { name: /create public link/i });
    expect(createBtn).toBeDisabled();

    await userEvent.click(
      screen.getByRole("checkbox", { name: /publicly accessible to anyone with the link/i }),
    );
    expect(createBtn).toBeEnabled();
  });

  it("POSTs create_embed with anonymous:true by default and refuses without confirmation", async () => {
    let body: Record<string, unknown> | null = null;
    // Stateful mock: the embeds list refetches after create and must show the new row.
    let embeds: Embed[] = [];
    server.use(
      http.get(`${BASE}/api/embeds`, () => HttpResponse.json(embeds)),
      http.post(`${BASE}/api/embeds`, async ({ request }) => {
        body = (await request.json()) as Record<string, unknown>;
        const created = makeEmbed();
        embeds = [created];
        return HttpResponse.json(created);
      }),
    );

    renderDialog();

    await userEvent.click(
      await screen.findByRole("checkbox", { name: /publicly accessible to anyone with the link/i }),
    );
    await userEvent.click(screen.getByRole("button", { name: /create public link/i }));

    expect(await screen.findByDisplayValue(makeEmbed().public_url!)).toBeInTheDocument();
    expect(body).toMatchObject({
      object_type: "visualization",
      object_id: 9,
      anonymous: true,
    });
  });

  it("shows a server-side error message when creation is refused (e.g. missing share_public_anonymous)", async () => {
    server.use(
      http.get(`${BASE}/api/embeds`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/embeds`, () =>
        HttpResponse.json({ message: "missing the share_public_anonymous permission" }, { status: 403 }),
      ),
    );

    renderDialog();

    await userEvent.click(
      await screen.findByRole("checkbox", { name: /publicly accessible to anyone with the link/i }),
    );
    await userEvent.click(screen.getByRole("button", { name: /create public link/i }));

    expect(
      await screen.findByText("missing the share_public_anonymous permission"),
    ).toBeInTheDocument();
  });

  it("keeps a long embed URL from forcing the dialog to overflow", async () => {
    // DialogContent is a grid; its child needs min-w-0 or the long unbroken URL forces the row
    // wider than the dialog.
    const longUrl = `http://localhost:3000/public/embed/${"a".repeat(64)}`;
    server.use(
      http.get(`${BASE}/api/embeds`, () => HttpResponse.json([makeEmbed({ public_url: longUrl })])),
    );

    renderDialog();

    await screen.findByDisplayValue(longUrl);

    const wrapper = screen.getByRole("dialog").querySelector(".min-w-0.space-y-4");
    expect(wrapper).not.toBeNull();
  });

  it("lists an existing active link with its iframe snippet and revokes it via DELETE", async () => {
    let deleted = false;
    let embeds: Embed[] = [makeEmbed()];
    server.use(
      http.get(`${BASE}/api/embeds`, () => HttpResponse.json(embeds)),
      http.delete(`${BASE}/api/embeds/1`, () => {
        deleted = true;
        embeds = [];
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderDialog();

    expect(await screen.findByDisplayValue(makeEmbed().public_url!)).toBeInTheDocument();
    expect(screen.getByText(/<iframe/)).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /revoke/i }));

    expect(deleted).toBe(true);
    expect(
      await screen.findByRole("button", { name: /^create public link$/i }),
    ).toBeInTheDocument();
  });

  it("shows a destructive message and keeps the link listed when revoke fails, instead of failing silently", async () => {
    server.use(
      http.get(`${BASE}/api/embeds`, () => HttpResponse.json([makeEmbed()])),
      http.delete(`${BASE}/api/embeds/1`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderDialog();

    await screen.findByDisplayValue(makeEmbed().public_url!);
    await userEvent.click(screen.getByRole("button", { name: /revoke/i }));

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
    expect(screen.getByDisplayValue(makeEmbed().public_url!)).toBeInTheDocument();
  });

  it("shows the destructive fallback message when revoke fails with no server-provided reason", async () => {
    server.use(
      http.get(`${BASE}/api/embeds`, () => HttpResponse.json([makeEmbed()])),
      http.delete(`${BASE}/api/embeds/1`, () => HttpResponse.error()),
    );

    renderDialog();
    await screen.findByDisplayValue(makeEmbed().public_url!);
    await userEvent.click(screen.getByRole("button", { name: /revoke/i }));

    expect(await screen.findByText(/may still be live/i)).toBeInTheDocument();
  });

  it("a successful revoke clears a prior error", async () => {
    let call = 0;
    let embeds: Embed[] = [makeEmbed()];
    server.use(
      http.get(`${BASE}/api/embeds`, () => HttpResponse.json(embeds)),
      http.delete(`${BASE}/api/embeds/1`, () => {
        call += 1;
        if (call === 1) return HttpResponse.json({ message: "database unavailable" }, { status: 500 });
        embeds = [];
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderDialog();
    await screen.findByDisplayValue(makeEmbed().public_url!);

    await userEvent.click(screen.getByRole("button", { name: /revoke/i }));
    expect(await screen.findByText("database unavailable")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /revoke/i }));
    await waitFor(() => expect(screen.queryByText("database unavailable")).not.toBeInTheDocument());
  });

  it("revoking one of two active links leaves the other's Revoke button enabled", async () => {
    const embeds = [makeEmbed({ id: 1 }), makeEmbed({ id: 2, token: "tok-two", public_url: "http://localhost:3000/public/embed/tok-two" })];
    server.use(
      http.get(`${BASE}/api/embeds`, () => HttpResponse.json(embeds)),
      http.delete(`${BASE}/api/embeds/1`, () => new Promise(() => {})),
    );

    renderDialog();
    await screen.findByDisplayValue(embeds[0].public_url!);

    const revokeButtons = screen.getAllByRole("button", { name: /revoke/i });
    await userEvent.click(revokeButtons[0]);

    await waitFor(() => expect(revokeButtons[0]).toBeDisabled());
    expect(revokeButtons[1]).toBeEnabled();
  });

  it("copying the link shows Copied on success", async () => {
    server.use(http.get(`${BASE}/api/embeds`, () => HttpResponse.json([makeEmbed()])));

    renderDialog();
    await screen.findByDisplayValue(makeEmbed().public_url!);
    await userEvent.click(screen.getByRole("button", { name: /^copy$/i }));

    expect(await screen.findByRole("button", { name: /^copied$/i })).toBeInTheDocument();
  });

  it("copying the link shows a manual-copy note when the clipboard is unavailable", async () => {
    Object.assign(navigator, { clipboard: undefined });
    server.use(http.get(`${BASE}/api/embeds`, () => HttpResponse.json([makeEmbed()])));

    renderDialog();
    await screen.findByDisplayValue(makeEmbed().public_url!);
    await userEvent.click(screen.getByRole("button", { name: /^copy$/i }));

    expect(await screen.findByText(/couldn.t copy/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^copied$/i })).not.toBeInTheDocument();
  });

  it("warns and disables Create when the object's visibility is restricted", async () => {
    server.use(http.get(`${BASE}/api/embeds`, () => HttpResponse.json([])));

    renderDialog(vi.fn(), "restricted");

    expect(
      await screen.findByText(/set this visualization's visibility to "everyone"/i),
    ).toBeInTheDocument();
    const createBtn = screen.getByRole("button", { name: /^create public link$/i });
    expect(createBtn).toBeDisabled();
    expect(createBtn).toHaveAttribute(
      "title",
      'Set this visualization\'s visibility to "Everyone" before creating a public link.',
    );

    await userEvent.click(
      screen.getByRole("checkbox", { name: /publicly accessible to anyone with the link/i }),
    );
    expect(createBtn).toBeDisabled();
  });

  it("shows no visibility warning and allows Create when the object is public", async () => {
    server.use(http.get(`${BASE}/api/embeds`, () => HttpResponse.json([])));

    renderDialog(vi.fn(), "workspace");

    await userEvent.click(
      await screen.findByRole("checkbox", { name: /publicly accessible to anyone with the link/i }),
    );
    expect(screen.queryByText(/set this visualization's visibility/i)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^create public link$/i })).toBeEnabled();
  });

  it("does not claim no public link while the embed list is still loading", async () => {
    server.use(http.get(`${BASE}/api/embeds`, () => new Promise(() => {})));

    renderDialog();

    expect(await screen.findByText(/checking for an existing public link/i)).toBeInTheDocument();
    expect(screen.queryByText(/no public link/i)).not.toBeInTheDocument();
  });

  it("does not render the create button before the embed list resolves", async () => {
    server.use(http.get(`${BASE}/api/embeds`, () => new Promise(() => {})));

    renderDialog();

    await screen.findByText(/checking for an existing public link/i);
    expect(screen.queryByRole("button", { name: /create public link/i })).not.toBeInTheDocument();
  });

  it("shows a load error instead of the misleading empty state when the embeds list fails to load", async () => {
    server.use(
      http.get(`${BASE}/api/embeds`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderDialog();

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
    expect(screen.queryByText(/no public link/i)).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /create public link/i })).not.toBeInTheDocument();
  });
});
