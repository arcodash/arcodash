import { useState, type ReactNode } from "react";
import { Link, NavLink, Outlet, useMatch, useNavigate } from "react-router";
import {
  Database,
  FileText,
  LayoutDashboard,
  Bell,
  ChevronLeft,
  LogOut,
  Menu,
  Settings,
  User as UserIcon,
  Building2,
  UsersRound,
  Activity,
  ScrollText,
  type LucideIcon,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { useAuth } from "@/lib/auth";
import { ACCESS_KEYS, NAV_PERMISSIONS, usePermissions } from "@/lib/permissions";
import { extensions } from "@/extensions";

const NAV_META: Record<string, { label: string; icon: LucideIcon }> = {
  "/queries": { label: "Queries", icon: FileText },
  "/data_sources": { label: "Data Sources", icon: Database },
  "/dashboards": { label: "Dashboards", icon: LayoutDashboard },
  "/monitors": { label: "Monitors", icon: Bell },
};

const NAV = NAV_PERMISSIONS.map((item) => ({ ...item, ...NAV_META[item.to] }));

const ADMIN_NAV: { to: string; label: string; icon: LucideIcon; anyOf: readonly string[] }[] = [
  { to: "/organization", label: "Organization", icon: Building2, anyOf: ["admin"] },
  { to: "/access", label: "Access Control", icon: UsersRound, anyOf: ACCESS_KEYS },
  { to: "/admin/status", label: "System Status", icon: Activity, anyOf: ["admin"] },
  { to: "/admin/audit", label: "Audit Log", icon: ScrollText, anyOf: ["admin"] },
];

const LOGO = <img src="/favicon.svg" alt="arcodash" width={20} height={20} className="shrink-0" />;

export function Wordmark({ className }: { className: string }) {
  return (
    <span className={className}>
      <span className="text-[#7e14ff]">arc</span>odash
    </span>
  );
}

function getInitials(name: string, email: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  if (words.length >= 2) return (words[0][0] + words[1][0]).toUpperCase();
  return (words[0] ?? email).slice(0, 2).toUpperCase();
}

/** `children` lets a route render the shell around non-child-route content (e.g. the signed-in
 * 404); defaults to `<Outlet />`. */
export function AppShell({ children }: { children?: ReactNode }) {
  const { user, logout } = useAuth();
  const navigate = useNavigate();
  // One-shot viewport read on mount; no matchMedia listener, so a later user toggle wins.
  const [collapsed, setCollapsed] = useState(
    () => !window.matchMedia("(min-width: 1024px)").matches,
  );
  const [mobileNavOpen, setMobileNavOpen] = useState(false);

  async function handleLogout() {
    // Navigate before clearing the session: logout() nulls the cached user, so RequireAuth would
    // redirect first and leak this page as `from`.
    navigate("/login", { replace: true });
    await logout();
  }

  return (
    <div className="flex h-svh flex-col text-foreground">
      {extensions.map((ext, i) => (ext.appChromeSlot ? <ext.appChromeSlot key={i} /> : null))}
      <div className="flex min-h-0 flex-1">
        {user && (
          <aside
            className={cn(
              "relative hidden h-full flex-col border-r border-sidebar-border bg-sidebar transition-[width] duration-200 ease-out motion-reduce:transition-none md:flex",
              collapsed ? "w-16" : "w-72",
            )}
          >
            {/* Clips labels as the rail narrows; the handle lives outside since it straddles the aside's border. */}
            <div className="flex h-full flex-col overflow-hidden">
              <div className="flex items-center px-2 py-3">
                <Link to="/queries" className="flex w-12 shrink-0 items-center justify-center">
                  {LOGO}
                </Link>
                <Wordmark
                  className={cn(
                    "min-w-0 flex-1 overflow-hidden whitespace-nowrap pr-3 text-sm font-semibold tracking-wide text-sidebar-foreground transition-opacity duration-200 ease-out motion-reduce:transition-none",
                    collapsed && "opacity-0",
                  )}
                />
              </div>

              <SidebarBody user={user} collapsed={collapsed} onLogout={handleLogout} />
            </div>

            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  aria-label={collapsed ? "Expand sidebar" : "Collapse sidebar"}
                  aria-expanded={!collapsed}
                  onClick={() => setCollapsed((v) => !v)}
                  className="absolute -right-3 top-2.5 z-10 flex h-6 w-6 items-center justify-center rounded-full border border-sidebar-border bg-sidebar text-muted-foreground shadow-sm hover:bg-sidebar-accent hover:text-sidebar-accent-foreground"
                >
                  <ChevronLeft
                    size={14}
                    className={cn(
                      "transition-transform duration-200 motion-reduce:transition-none",
                      collapsed && "rotate-180",
                    )}
                  />
                </button>
              </TooltipTrigger>
              <TooltipContent side="right">
                {collapsed ? "Expand sidebar" : "Collapse sidebar"}
              </TooltipContent>
            </Tooltip>
          </aside>
        )}

        {user && (
          <Sheet open={mobileNavOpen} onOpenChange={setMobileNavOpen}>
            <SheetContent side="left" className="flex w-72 flex-col p-0">
              <SheetTitle className="sr-only">Navigation</SheetTitle>
              <div className="flex items-center gap-2 p-3">
                {LOGO}
                <Wordmark className="text-sm font-semibold tracking-wide text-sidebar-foreground" />
              </div>

              <SidebarBody
                user={user}
                collapsed={false}
                // A useBlocker-cancelled navigation still closes this drawer; accepted, reopening is
                // one tap.
                onNavigate={() => setMobileNavOpen(false)}
                onLogout={handleLogout}
              />
            </SheetContent>
          </Sheet>
        )}

        <main className="flex min-w-0 flex-1 flex-col">
          <header className="flex items-center justify-between border-b border-border bg-background px-4 py-4 md:px-6">
            <div className="flex min-w-0 items-center gap-3">
              {user && (
                <div className="flex items-center gap-2 md:hidden">
                  <Button
                    variant="ghost"
                    size="icon"
                    aria-label="Open navigation"
                    onClick={() => setMobileNavOpen(true)}
                  >
                    <Menu size={18} />
                  </Button>
                  {LOGO}
                  <Wordmark className="text-sm font-semibold tracking-wide text-foreground" />
                </div>
              )}
              <div className="text-sm font-medium text-muted-foreground">
                {extensions.map((ext, i) => (ext.orgSwitcherSlot ? <ext.orgSwitcherSlot key={i} /> : null))}
              </div>
            </div>
          </header>

          {/* scrollbar-gutter:stable keeps layout from jumping when content barely straddles a container-query breakpoint. */}
          <div className="flex-1 overflow-y-auto px-4 py-4 md:px-6 md:py-6 [scrollbar-gutter:stable]">
            {/* Single content gutter for authenticated pages; a page opts into a narrower column with its own `max-w-*` (no `mx-auto`). */}
            <div className="mx-auto w-full max-w-6xl">{children ?? <Outlet />}</div>
          </div>
        </main>
      </div>
    </div>
  );
}

/** Nav, extension footer slot and account menu, shared by the desktop rail and the mobile drawer.
 * `onNavigate` runs after any navigation or sign-out (the drawer closes itself with it). */
function SidebarBody({
  user,
  collapsed,
  onNavigate,
  onLogout,
}: {
  user: { name: string; email: string };
  collapsed: boolean;
  onNavigate?: () => void;
  onLogout: () => void;
}) {
  const { hasAny } = usePermissions();
  const navigate = useNavigate();
  // /destinations stays in NAV_PERMISSIONS so computeLandingPath can route manage-only users there,
  // but isn't a sidebar item (no NAV_META).
  const visibleNav = NAV.filter((item) => hasAny(item.anyOf) && item.label);
  const visibleAdminNav = ADMIN_NAV.filter((item) => hasAny(item.anyOf));
  // A slot can still render null; this only rules out the wrapper when no extension offers one.
  const hasSidebarFooterSlot = extensions.some((ext) => ext.sidebarFooterSlot);
  const navItem = ({ to, label, icon }: { to: string; label: string; icon: LucideIcon }) => (
    <NavItem key={to} to={to} label={label} Icon={icon} collapsed={collapsed} onClick={onNavigate} />
  );

  return (
    <>
      <nav className="custom-scroll flex-1 space-y-1 overflow-y-auto overflow-x-hidden px-2 pb-3">
        {visibleNav.map(navItem)}
        {visibleAdminNav.length > 0 && (
          <div className="mt-4 border-t border-sidebar-border pt-3">
            <span
              className={cn(
                "block overflow-hidden whitespace-nowrap px-3 pb-1 text-xs font-medium uppercase tracking-wide text-muted-foreground transition-opacity duration-200 ease-out motion-reduce:transition-none",
                collapsed && "opacity-0",
              )}
            >
              Admin
            </span>
            {visibleAdminNav.map(navItem)}
          </div>
        )}
      </nav>

      {hasSidebarFooterSlot && (
        <div className="px-2 pb-2">
          {extensions.map((ext, i) =>
            ext.sidebarFooterSlot ? <ext.sidebarFooterSlot key={i} collapsed={collapsed} /> : null,
          )}
        </div>
      )}

      <div className="border-t border-sidebar-border p-2">
        <AccountMenu
          user={user}
          collapsed={collapsed}
          onNavigate={(to) => {
            navigate(to);
            onNavigate?.();
          }}
          onLogout={() => {
            onLogout();
            onNavigate?.();
          }}
        />
      </div>
    </>
  );
}

function AccountMenu({
  user,
  collapsed,
  onNavigate,
  onLogout,
}: {
  user: { name: string; email: string };
  collapsed: boolean;
  onNavigate: (to: string) => void;
  onLogout: () => void;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className="flex w-full cursor-pointer items-center rounded-md px-0 py-2 text-left transition hover:bg-sidebar-accent"
        >
          <span className="flex w-12 shrink-0 items-center justify-center">
            <Avatar className="h-9 w-9 shrink-0 rounded-lg">
              <AvatarFallback className="rounded-lg bg-muted text-sm font-semibold text-muted-foreground">
                {getInitials(user.name, user.email)}
              </AvatarFallback>
            </Avatar>
          </span>
          <div
            className={cn(
              "grid min-w-0 flex-1 overflow-hidden pr-3 text-left text-sm leading-tight transition-opacity duration-200 ease-out motion-reduce:transition-none",
              collapsed && "opacity-0",
            )}
          >
            <span className="truncate font-medium text-sidebar-foreground">{user.name}</span>
            <span className="truncate text-xs text-muted-foreground">{user.email}</span>
          </div>
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent side="right" align="end" className="w-56">
        <DropdownMenuLabel className="text-xs uppercase text-muted-foreground">Account</DropdownMenuLabel>
        <div className="px-2 py-1.5 text-sm">
          <p className="truncate font-medium">{user.name}</p>
          <p className="truncate text-xs text-muted-foreground">{user.email}</p>
        </div>
        <DropdownMenuSeparator />
        <DropdownMenuItem onClick={() => onNavigate("/account")}>
          <UserIcon size={16} className="mr-2" />
          Account
        </DropdownMenuItem>
        <DropdownMenuItem onClick={() => onNavigate("/settings")}>
          <Settings size={16} className="mr-2" />
          Settings
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          className="text-destructive focus:bg-destructive/10 focus:text-destructive"
          onClick={onLogout}
        >
          <LogOut size={16} className="mr-2" />
          Sign out
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function NavItem({
  to,
  label,
  Icon,
  collapsed,
  onClick,
}: {
  to: string;
  label: string;
  Icon: LucideIcon;
  collapsed: boolean;
  onClick?: () => void;
}) {
  // A plain string className, not NavLink's function form: Radix Tooltip asChild merges className
  // by concatenation.
  const isActive = !!useMatch({ path: to, end: to === "/queries" });

  const link = (
    <NavLink
      to={to}
      onClick={onClick}
      className={cn(
        "flex items-center rounded-md px-0 py-2 text-sm transition",
        isActive
          ? "bg-sidebar-accent text-sidebar-accent-foreground font-medium"
          : "text-sidebar-foreground hover:bg-sidebar-accent hover:text-sidebar-accent-foreground",
      )}
      end={to === "/queries"}
    >
      <span className="flex w-12 shrink-0 items-center justify-center">
        <Icon size={16} className="shrink-0" />
      </span>
      <span
        className={cn(
          "min-w-0 flex-1 overflow-hidden whitespace-nowrap pr-3 transition-opacity duration-200 ease-out motion-reduce:transition-none",
          collapsed && "opacity-0",
        )}
      >
        {label}
      </span>
    </NavLink>
  );

  // asChild merges onto the same NavLink in both states, so the DOM never remounts (which would
  // kill the label opacity transition).
  return (
    <Tooltip>
      <TooltipTrigger asChild>{link}</TooltipTrigger>
      {collapsed && (
        <TooltipContent side="right" sideOffset={8}>
          {label}
        </TooltipContent>
      )}
    </Tooltip>
  );
}
