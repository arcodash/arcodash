import { describe, it, expect } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import {
  PERMISSION_KEYS,
  ACCESS_KEYS,
  NAV_PERMISSIONS,
  usePermissions,
  computeLandingPath,
} from "@/lib/permissions";
import { WORKSPACE_ITEMS, SYSTEM_ITEMS } from "@/pages/SettingsPage";
import { TABS } from "@/pages/AccessControlPage";

const BASE = "http://localhost:8080";

/** Mirrors `crates/shared/src/permissions_catalog.rs::CATALOG` — the backend's own
 * `catalog_covers_the_documented_permission_set` test pins the same 31-key list on that side. */
const EXPECTED_CATALOG = [
  "admin",
  "view_users",
  "manage_users",
  "view_groups",
  "manage_groups",
  "view_roles",
  "manage_roles",
  "view_dashboard",
  "create_dashboard",
  "edit_dashboard",
  "manage_dashboard",
  "view_query",
  "create_query",
  "view_snippets",
  "manage_snippets",
  "edit_query",
  "execute_query",
  "schedule_query",
  "manage_query",
  "view_data_source",
  "create_data_source",
  "edit_data_source",
  "manage_data_source",
  "view_monitor",
  "create_monitor",
  "edit_monitor",
  "manage_monitor",
  "view_destinations",
  "manage_destinations",
  "create_embed",
  "share_public_anonymous",
];

function renderWithClient(ui: React.ReactNode) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

function sessionWith(permissions: string[], isAdmin: boolean) {
  return http.get(`${BASE}/api/session`, () =>
    HttpResponse.json({
      user: { id: 1, name: "Test User", email: "t@example.com" },
      org: { id: 1, name: "Org", slug: "org" },
      permissions,
      is_admin: isAdmin,
    }),
  );
}

describe("PERMISSION_KEYS", () => {
  it("matches the backend catalog exactly", () => {
    expect([...PERMISSION_KEYS].sort()).toEqual([...EXPECTED_CATALOG].sort());
    expect(PERMISSION_KEYS.length).toBe(31);
  });
});

describe("permission maps reference only catalog keys", () => {
  const catalog = new Set<string>(PERMISSION_KEYS);

  it("NAV_PERMISSIONS", () => {
    for (const item of NAV_PERMISSIONS) {
      for (const key of item.anyOf) expect(catalog.has(key)).toBe(true);
    }
  });

  it("ACCESS_KEYS", () => {
    for (const key of ACCESS_KEYS) expect(catalog.has(key)).toBe(true);
  });

  it("SettingsPage WORKSPACE_ITEMS / SYSTEM_ITEMS", () => {
    for (const item of [...WORKSPACE_ITEMS, ...SYSTEM_ITEMS]) {
      for (const key of item.anyOf) expect(catalog.has(key)).toBe(true);
    }
  });

  it("AccessControlPage TABS", () => {
    for (const tab of TABS) {
      for (const key of tab.anyOf) expect(catalog.has(key)).toBe(true);
    }
  });
});

describe("computeLandingPath", () => {
  it("sends a manage_destinations-only caller to /destinations, not /monitors", () => {
    const hasAny = (keys: readonly string[]) => keys.includes("manage_destinations");
    expect(computeLandingPath(hasAny)).toBe("/destinations");
  });

  it("sends a view_monitor-only caller to /monitors", () => {
    const hasAny = (keys: readonly string[]) => keys.includes("view_monitor");
    expect(computeLandingPath(hasAny)).toBe("/monitors");
  });
});

describe("usePermissions", () => {
  function Probe({ checkKey }: { checkKey: string }) {
    const { has, isAdmin, isLoading } = usePermissions();
    if (isLoading) return <p>loading</p>;
    return (
      <p>
        has:{String(has(checkKey))} admin:{String(isAdmin)}
      </p>
    );
  }

  it("admin implies every key even when not literally held", async () => {
    server.use(sessionWith([], true));
    renderWithClient(<Probe checkKey="manage_roles" />);
    await waitFor(() => expect(screen.getByText(/has:true admin:true/)).toBeInTheDocument());
  });

  it("a non-admin only has literal (server-expanded) keys", async () => {
    server.use(sessionWith(["view_query"], false));
    renderWithClient(<Probe checkKey="edit_query" />);
    await waitFor(() => expect(screen.getByText(/has:false admin:false/)).toBeInTheDocument());
  });
});
