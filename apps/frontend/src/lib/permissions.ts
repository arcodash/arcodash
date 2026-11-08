import { useQuery } from "@tanstack/react-query";
import { api } from "@/lib/api";
import { SESSION_QUERY_KEY } from "@/lib/sessionQueryKey";

/** Mirrors `crates/shared/src/permissions_catalog.rs::CATALOG` key-for-key; `permissions.test.tsx`
 * pins it so drift fails loudly. */
export const PERMISSION_KEYS = [
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
] as const;

const KNOWN_KEYS = new Set<string>(PERMISSION_KEYS);

export const ACCESS_KEYS = [
  "admin",
  "view_users",
  "manage_users",
  "view_groups",
  "manage_groups",
  "view_roles",
  "manage_roles",
] as const;

/** Sidebar nav entries in priority order; also drives the landing page. Icon/label live in
 * `AppShell.tsx`. */
export const NAV_PERMISSIONS: { to: string; anyOf: string[] }[] = [
  { to: "/queries", anyOf: ["view_query", "create_query", "edit_query", "execute_query"] },
  { to: "/data_sources", anyOf: ["view_data_source"] },
  { to: "/dashboards", anyOf: ["view_dashboard", "create_dashboard", "edit_dashboard"] },
  { to: "/destinations", anyOf: ["view_destinations", "manage_destinations"] },
  { to: "/monitors", anyOf: ["view_monitor"] },
];

export function computeLandingPath(hasAny: (keys: readonly string[]) => boolean): string {
  return NAV_PERMISSIONS.find((item) => hasAny(item.anyOf))?.to ?? "/settings";
}

export interface Permissions {
  keys: Set<string>;
  isAdmin: boolean;
  isLoading: boolean;
  has: (key: string) => boolean;
  hasAny: (keys: readonly string[]) => boolean;
}

/** Reads the `/api/session` query `AuthProvider` already blocks on (shared queryKey dedupes fetches). `admin`
 * short-circuits every check; the server pre-expands `manage_*`, so no implies logic lives here. */
export function usePermissions(): Permissions {
  const { data, isLoading } = useQuery({
    queryKey: SESSION_QUERY_KEY,
    queryFn: () => api.getSession(),
    retry: false,
  });

  const keys = new Set(data?.permissions ?? []);
  const isAdmin = data?.is_admin ?? false;

  function has(key: string): boolean {
    if (import.meta.env.DEV && !KNOWN_KEYS.has(key)) {
      console.warn(`usePermissions.has: unknown permission key "${key}"`);
    }
    return isAdmin || keys.has(key);
  }

  function hasAny(list: readonly string[]): boolean {
    return list.some(has);
  }

  return { keys, isAdmin, isLoading, has, hasAny };
}
