// Extension slots, empty by default. A deployment fills them in via the `@/extensions` Vite alias
// (`ARCODASH_EXT` env var, see vite.config.ts).
import type { ComponentType, ReactNode } from "react";
import type { RouteObject } from "react-router";

export interface AppExtension {
  /** Reserved: not currently read by AppShell. */
  navItems: { to: string; label: string; icon: ComponentType }[];
  routes: RouteObject[];
  /** Routes mounted as children of the app-shell layout route so they share one `AppShell`
   * instance. Paths are relative to `/`. */
  layoutRoutes?: RouteObject[];
  /** Sign-in UI; renders above the built-in form, or alone when the server reports
   * `password_auth_enabled: false`. */
  loginSlot?: ComponentType;
  orgSwitcherSlot?: ComponentType;
  /** Rendered once at the top of the authenticated app shell (e.g. a banner). */
  appChromeSlot?: ComponentType;
  /** Rendered above the account-menu divider in desktop and mobile nav; takes the sidebar's
   * collapsed state. */
  sidebarFooterSlot?: ComponentType<{ collapsed: boolean }>;
  /** Wraps login/signup/verify/invite-accept content; default centered layout otherwise. */
  authShellSlot?: ComponentType<{ children: ReactNode }>;
  /** Called after `logout()` clears its state so an extension can drop session-scoped state before
   * the next login. */
  onLogout?: () => void;
  /** Extra cards for `SettingsPage`'s Workspace group; built-in `WORKSPACE_ITEMS` cannot reference
   * an extension-only route. */
  settingsWorkspaceItems?: {
    to: string;
    label: string;
    icon: ComponentType<{ className?: string }>;
    description: string;
    anyOf: readonly string[];
  }[];
}

export const extensions: AppExtension[] = [];
