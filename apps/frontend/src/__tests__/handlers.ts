import { http, HttpResponse } from "msw";
import { PERMISSION_KEYS } from "@/lib/permissions";

const BASE = "http://localhost:8080";

// Default happy-path handlers. Tests override these with server.use() per case. Session defaults
// to a full admin so tests that assume unrestricted access need no override of their own — tests
// that specifically exercise a limited-permission user override this handler per `server.use()`.
export const handlers = [
  http.get(`${BASE}/api/session`, () =>
    HttpResponse.json({
      user: {
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
      },
      org: { id: 1, name: "Test Org", slug: "test-org", settings: {} },
      permissions: [...PERMISSION_KEYS],
      is_admin: true,
    }),
  ),

  http.post(`${BASE}/api/login`, () =>
    HttpResponse.json({
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
    }),
  ),

  http.get(`${BASE}/api/data_sources`, () =>
    HttpResponse.json({ data_sources: [], can_create: true }),
  ),

  http.get(`${BASE}/api/queries`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/queries/tags`, () => HttpResponse.json({ tags: [] })),

  http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/queries/:id/visualizations`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/schedules`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/monitors`, () => HttpResponse.json([])),
  http.get(`${BASE}/api/monitors/:id/observations`, () => HttpResponse.json([])),
  http.get(`${BASE}/api/queries/:id/results`, () => new HttpResponse(null, { status: 404 })),

  http.get(`${BASE}/api/runs`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/groups`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/destinations`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/destinations/types`, () =>
    HttpResponse.json([
      {
        type: "email",
        name: "Email",
        icon: "fa-envelope",
        configuration_schema: { to: { type: "string", title: "To" } },
        secret: [],
      },
      {
        type: "slack",
        name: "Slack",
        icon: "fa-slack",
        configuration_schema: { url: { type: "string", title: "Webhook URL" } },
        secret: ["url"],
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
    ]),
  ),

  http.get(`${BASE}/api/users`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/permissions`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/roles`, () => HttpResponse.json([])),

  http.get(`${BASE}/api/users/:id/effective_permissions`, () =>
    HttpResponse.json({ permissions: [], is_admin: false }),
  ),
];
