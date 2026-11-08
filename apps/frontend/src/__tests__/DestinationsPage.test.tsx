import { describe, it, expect } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { DestinationsPage } from "@/pages/DestinationsPage";

const BASE = "http://localhost:8080";

const TYPES = [
  {
    type: "email",
    name: "Email",
    icon: "fa-envelope",
    configuration_schema: { to: { type: "string", title: "To" } },
    secret: [],
  },
  {
    type: "webhook",
    name: "Webhook",
    icon: "fa-bolt",
    configuration_schema: {
      url: { type: "string", title: "URL" },
      username: { type: "string", title: "Username" },
    },
    secret: ["url"],
  },
  {
    type: "microsoft_teams_webhook",
    name: "Microsoft Teams",
    icon: "fa-windows",
    configuration_schema: { url: { type: "string", title: "Webhook URL" } },
    secret: ["url"],
  },
];

const WEBHOOK_DEST = {
  id: 1,
  name: "Ops Webhook",
  type: "webhook",
  options: { username: "old" }, // url is a secret, never echoed by the real API either
  icon: "fa-bolt",
  created_at: "2025-01-01T00:00:00Z",
};

const TEAMS_DEST = {
  id: 2,
  name: "Teams Alert",
  type: "microsoft_teams_webhook",
  options: {},
  icon: "fa-windows",
  created_at: "2025-01-01T00:00:00Z",
};

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <DestinationsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function mockTypes() {
  server.use(http.get(`${BASE}/api/destinations/types`, () => HttpResponse.json(TYPES)));
}

/** Default session is full-admin; override to a non-manager for read-only gating. */
function nonManagerSession() {
  server.use(
    http.get(`${BASE}/api/session`, () =>
      HttpResponse.json({
        user: { id: 1, name: "Test User", email: "test@example.com" },
        org: { id: 1, name: "Org", slug: "org" },
        permissions: ["view_monitor", "view_destinations"],
        is_admin: false,
      }),
    ),
  );
}

describe("DestinationsPage", () => {
  it("renders the page heading above the Monitors/Destinations tab bar, not beneath it", async () => {
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])));
    renderPage();

    const heading = await screen.findByRole("heading", { name: "Notification destinations" });
    const tab = await screen.findByRole("tab", { name: "Destinations" });
    expect(heading.compareDocumentPosition(tab) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("shows the type display name, not the raw type slug", async () => {
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([TEAMS_DEST])));

    renderPage();

    expect(await screen.findByText("Teams Alert")).toBeInTheDocument();
    expect(screen.getByText("Microsoft Teams")).toBeInTheDocument();
    expect(screen.queryByText("microsoft_teams_webhook")).not.toBeInTheDocument();
  });

  it("shows an EmptyState with an action when there are no destinations", async () => {
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])));

    renderPage();

    expect(await screen.findByText("Add your first destination")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: /new destination/i })).toHaveLength(2);
  });

  it("edit dialog prefills name, shows the type as a read-only field, and leaves secrets empty", async () => {
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([WEBHOOK_DEST])));

    renderPage();
    await screen.findByText("Ops Webhook");

    await userEvent.click(screen.getByRole("button", { name: /edit ops webhook/i }));

    const nameInput = await screen.findByDisplayValue("Ops Webhook");
    expect(nameInput).toBeInTheDocument();

    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText("Webhook")).toBeInTheDocument();
    expect(screen.queryByDisplayValue("Webhook")).not.toBeInTheDocument();

    const usernameInput = screen.getByDisplayValue("old");
    expect(usernameInput).toBeInTheDocument();

    const urlInput = screen.getByPlaceholderText("Leave blank to keep current");
    expect(urlInput).toHaveValue("");
    expect(urlInput).toHaveAttribute("type", "password");
  });

  it("the reveal toggle flips a secret field's input type without ever populating a stored value", async () => {
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([WEBHOOK_DEST])));

    renderPage();
    await screen.findByText("Ops Webhook");
    await userEvent.click(screen.getByRole("button", { name: /edit ops webhook/i }));

    const urlInput = await screen.findByPlaceholderText("Leave blank to keep current");
    expect(urlInput).toHaveAttribute("type", "password");

    const toggle = screen.getByRole("button", { name: /show url/i });
    await userEvent.click(toggle);
    expect(urlInput).toHaveAttribute("type", "text");
    // The stored secret is never returned by the API; reveal must show only what the user typed.
    expect(urlInput).toHaveValue("");

    await userEvent.click(screen.getByRole("button", { name: /hide url/i }));
    expect(urlInput).toHaveAttribute("type", "password");

    await userEvent.type(urlInput, "https://hooks.example.com/abc");
    await userEvent.click(screen.getByRole("button", { name: /show url/i }));
    expect(urlInput).toHaveValue("https://hooks.example.com/abc");
  });

  it("a rename-only save sends {name} with no options and no type key", async () => {
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([WEBHOOK_DEST])));

    let postedBody: unknown = null;
    server.use(
      http.post(`${BASE}/api/destinations/1`, async ({ request }) => {
        postedBody = await request.json();
        return HttpResponse.json({ ...WEBHOOK_DEST, name: "Renamed" });
      }),
    );

    renderPage();
    await screen.findByText("Ops Webhook");
    await userEvent.click(screen.getByRole("button", { name: /edit ops webhook/i }));

    const nameInput = await screen.findByDisplayValue("Ops Webhook");
    await userEvent.clear(nameInput);
    await userEvent.type(nameInput, "Renamed");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => expect(postedBody).toEqual({ name: "Renamed" }));
  });

  it("touching an option strips blank fields (including a blank secret) from the body", async () => {
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([WEBHOOK_DEST])));

    let postedBody: unknown = null;
    server.use(
      http.post(`${BASE}/api/destinations/1`, async ({ request }) => {
        postedBody = await request.json();
        return HttpResponse.json(WEBHOOK_DEST);
      }),
    );

    renderPage();
    await screen.findByText("Ops Webhook");
    await userEvent.click(screen.getByRole("button", { name: /edit ops webhook/i }));

    const usernameInput = await screen.findByDisplayValue("old");
    await userEvent.clear(usernameInput);
    await userEvent.type(usernameInput, "new");
    await userEvent.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() =>
      expect(postedBody).toEqual({ name: "Ops Webhook", options: { username: "new" } }),
    );
  });

  it("delete only fires DELETE after confirming in the dialog", async () => {
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([WEBHOOK_DEST])));

    let deleteCalls = 0;
    server.use(
      http.delete(`${BASE}/api/destinations/1`, () => {
        deleteCalls += 1;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage();
    await screen.findByText("Ops Webhook");

    await userEvent.click(screen.getByRole("button", { name: /delete ops webhook/i }));
    expect(await screen.findByText("Delete destination?")).toBeInTheDocument();
    expect(deleteCalls).toBe(0);

    await userEvent.click(screen.getByRole("button", { name: /^cancel$/i }));
    expect(deleteCalls).toBe(0);

    await userEvent.click(screen.getByRole("button", { name: /delete ops webhook/i }));
    await userEvent.click(screen.getByRole("button", { name: /^delete$/i }));

    await waitFor(() => expect(deleteCalls).toBe(1));
  });
});

describe("DestinationsPage — non-manager (read-only)", () => {
  it("hides all write controls and shows the Monitors/Destinations tab strip", async () => {
    nonManagerSession();
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([WEBHOOK_DEST])));

    renderPage();
    await screen.findByText("Ops Webhook");

    expect(screen.queryByRole("button", { name: /new destination/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /edit ops webhook/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /delete ops webhook/i })).not.toBeInTheDocument();
    expect(screen.queryByText("Actions")).not.toBeInTheDocument();

    expect(screen.getByRole("tab", { name: "Monitors" })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Destinations" })).toBeInTheDocument();
  });

  it("shows a plain read-only line instead of the create-CTA EmptyState when the list is empty", async () => {
    nonManagerSession();
    mockTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])));

    renderPage();

    expect(
      await screen.findByText("No notification destinations have been set up yet."),
    ).toBeInTheDocument();
    expect(screen.queryByText("Add your first destination")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /new destination/i })).not.toBeInTheDocument();
  });
});

const TYPES_WITH_REQUIRED = [
  {
    type: "email",
    name: "Email",
    icon: "fa-envelope",
    configuration_schema: { to: { type: "string", title: "To" } },
    secret: [],
    required: ["to"],
  },
  {
    type: "webhook",
    name: "Webhook",
    icon: "fa-bolt",
    // Generic Webhook has only `url`; Username/Icon Emoji are Slack/Mattermost-only.
    configuration_schema: { url: { type: "string", title: "URL" } },
    secret: ["url"],
    required: ["url"],
  },
];

function mockRequiredTypes() {
  server.use(http.get(`${BASE}/api/destinations/types`, () => HttpResponse.json(TYPES_WITH_REQUIRED)));
}

describe("DestinationsPage per-type fields, required marks, test-send, target", () => {
  it("New Destination renders only Webhook's own fields — no Slack-only Username/Icon Emoji", async () => {
    mockRequiredTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])));
    const user = userEvent.setup();
    renderPage();

    await user.click((await screen.findAllByRole("button", { name: /new destination/i }))[0]);
    await user.click(screen.getByRole("combobox", { name: /type/i }));
    await user.click(await screen.findByRole("option", { name: "Webhook" }));

    expect(await screen.findByLabelText(/^URL/)).toBeInTheDocument();
    expect(screen.queryByLabelText(/Username/)).not.toBeInTheDocument();
    expect(screen.queryByLabelText(/Icon Emoji/)).not.toBeInTheDocument();
  });

  it("marks Name and every type.required field with a required marker, and validates on submit", async () => {
    mockRequiredTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])));
    const user = userEvent.setup();
    renderPage();

    await user.click((await screen.findAllByRole("button", { name: /new destination/i }))[0]);
    await user.click(screen.getByRole("combobox", { name: /type/i }));
    await user.click(await screen.findByRole("option", { name: "Webhook" }));

    expect(screen.getByText("Name").closest("label")).toHaveTextContent("(required)");
    expect(screen.getByText("URL").closest("label")).toHaveTextContent("(required)");

    await user.click(screen.getByRole("button", { name: "Create" }));
    expect(await screen.findByText("URL is required.")).toBeInTheDocument();
  });

  it("blocks creating an email destination without a valid To address", async () => {
    mockRequiredTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])));
    let posted = false;
    server.use(
      http.post(`${BASE}/api/destinations`, () => {
        posted = true;
        return HttpResponse.json({});
      }),
    );
    const user = userEvent.setup();
    renderPage();

    await user.click((await screen.findAllByRole("button", { name: /new destination/i }))[0]);
    await user.click(screen.getByRole("combobox", { name: /type/i }));
    await user.click(await screen.findByRole("option", { name: "Email" }));
    await user.type(screen.getByLabelText(/^Name/), "Ops");

    await user.click(screen.getByRole("button", { name: "Create" }));
    expect(await screen.findByText("To is required.")).toBeInTheDocument();

    await user.type(screen.getByLabelText(/^To/), "not-an-email");
    await user.click(screen.getByRole("button", { name: "Create" }));
    expect(await screen.findByText("Enter a valid email address.")).toBeInTheDocument();
    expect(posted).toBe(false);
  });

  it("shows a Send test message action in the New Destination dialog footer", async () => {
    mockRequiredTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])));
    let testedBody: unknown = null;
    server.use(
      http.post(`${BASE}/api/destinations/test`, async ({ request }) => {
        testedBody = await request.json();
        return HttpResponse.json({ message: "Test message sent" });
      }),
    );
    const user = userEvent.setup();
    renderPage();

    await user.click((await screen.findAllByRole("button", { name: /new destination/i }))[0]);
    await user.click(screen.getByRole("combobox", { name: /type/i }));
    await user.click(await screen.findByRole("option", { name: "Webhook" }));
    await user.type(await screen.findByLabelText(/^URL/), "https://hooks.example.com/abc");

    await user.click(screen.getByRole("button", { name: "Send test message" }));

    await waitFor(() => expect(testedBody).toEqual({ type: "webhook", options: { url: "https://hooks.example.com/abc" } }));
    expect(await screen.findByText("Test message sent")).toBeInTheDocument();
  });

  it("a successful create posts the entered destination", async () => {
    mockRequiredTypes();
    server.use(http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])));
    let createdBody: unknown = null;
    server.use(
      http.post(`${BASE}/api/destinations`, async ({ request }) => {
        createdBody = await request.json();
        return HttpResponse.json({
          id: 9,
          name: "Ops Webhook",
          type: "webhook",
          options: {},
          icon: "fa-bolt",
          created_at: "2025-01-01T00:00:00Z",
        });
      }),
    );
    const user = userEvent.setup();
    renderPage();

    await user.click((await screen.findAllByRole("button", { name: /new destination/i }))[0]);
    await user.type(screen.getByLabelText(/^Name/), "Ops Webhook");
    await user.click(screen.getByRole("combobox", { name: /type/i }));
    await user.click(await screen.findByRole("option", { name: "Webhook" }));
    await user.type(await screen.findByLabelText(/^URL/), "https://hooks.example.com/abc");
    await user.click(screen.getByRole("button", { name: "Create" }));

    await waitFor(() => expect(createdBody).not.toBeNull());
  });

  it("lists the redacted Target column and offers a row-level test-send action", async () => {
    mockRequiredTypes();
    server.use(
      http.get(`${BASE}/api/destinations`, () =>
        HttpResponse.json([
          {
            id: 1,
            name: "Ops Webhook",
            type: "webhook",
            options: {},
            icon: "fa-bolt",
            created_at: "2025-01-01T00:00:00Z",
            target: "https://hooks.example.com/…",
          },
        ]),
      ),
    );

    renderPage();

    expect(await screen.findByText("https://hooks.example.com/…")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send test message" })).toBeInTheDocument();
  });

  it("row-level test-send shows a transient Sent/Failed result", async () => {
    mockRequiredTypes();
    server.use(
      http.get(`${BASE}/api/destinations`, () =>
        HttpResponse.json([
          {
            id: 1,
            name: "Ops Webhook",
            type: "webhook",
            options: {},
            icon: "fa-bolt",
            created_at: "2025-01-01T00:00:00Z",
            target: "https://hooks.example.com/…",
          },
        ]),
      ),
    );
    server.use(
      http.post(`${BASE}/api/destinations/test`, () => HttpResponse.json({ message: "Test message sent" })),
    );

    const user = userEvent.setup();
    renderPage();
    await screen.findByText("Ops Webhook");

    await user.click(screen.getByRole("button", { name: "Send test message" }));
    expect(await screen.findByText("Sent")).toBeInTheDocument();
  });
});
