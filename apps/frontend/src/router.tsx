import { lazy, Suspense, type ReactNode } from "react";
import { createBrowserRouter, Navigate, useLocation, useParams } from "react-router";
import { AppShell } from "@/components/AppShell";
import { Button } from "@/components/ui/button";
import { RequireAuth, RequirePermission, useAuth } from "@/lib/auth";
import { ACCESS_KEYS, computeLandingPath, usePermissions } from "@/lib/permissions";
import { useDocumentTitle } from "@/lib/useDocumentTitle";
import { PageLoader } from "@/components/ui/loader";
import { LoginPage } from "@/pages/LoginPage";
import { QueriesPage } from "@/pages/QueriesPage";
import { DataSourcesPage } from "@/pages/DataSourcesPage";
import { DataSourceNewPage } from "@/pages/DataSourceNewPage";
import { DataSourceDetailPage } from "@/pages/DataSourceDetailPage";
import { DataSourceEditPage } from "@/pages/DataSourceEditPage";
import { DashboardsPage } from "@/pages/DashboardsPage";
import { MonitorsPage } from "@/pages/MonitorsPage";
import { MonitorPage } from "@/pages/MonitorPage";
import { DestinationsPage } from "@/pages/DestinationsPage";
import { InviteAcceptPage } from "@/pages/InviteAcceptPage";
import { ForgotPasswordPage } from "@/pages/ForgotPasswordPage";
import { ResetPasswordPage } from "@/pages/ResetPasswordPage";
import AccessControlPage from "@/pages/AccessControlPage";
import OrganizationPage from "@/pages/OrganizationPage";
import AiSettingsPage from "@/pages/AiSettingsPage";
import SystemStatusPage from "@/pages/SystemStatusPage";
import AuditLogPage from "@/pages/AuditLogPage";
import { SettingsPage } from "@/pages/SettingsPage";
import { AccountPage } from "@/pages/AccountPage";
import NotificationSettingsPage from "@/pages/NotificationSettingsPage";
import { NotFoundPage } from "@/pages/NotFoundPage";
import { extensions } from "@/extensions";

// Deferred: each pulls in CodeMirror, recharts or react-grid-layout, which would bloat the entry
// chunk (incl. /login and public embeds).
const QueryEditorPage = lazy(() =>
  import("@/pages/QueryEditorPage").then((m) => ({ default: m.QueryEditorPage })),
);
const DashboardPage = lazy(() =>
  import("@/pages/DashboardPage").then((m) => ({ default: m.DashboardPage })),
);
const PublicDashboardPage = lazy(() =>
  import("@/pages/PublicDashboardPage").then((m) => ({ default: m.PublicDashboardPage })),
);
const PublicEmbedPage = lazy(() =>
  import("@/pages/PublicEmbedPage").then((m) => ({ default: m.PublicEmbedPage })),
);
// Pulls in Recharts; kept out of the entry chunk.
const AiUsagePage = lazy(() => import("@/pages/AiUsagePage"));

/** Sets the browser title for a static route; pages that fetch the object they name (QueryEditorPage)
 * override it in-place so two effects do not fight over `document.title`. */
function RouteTitle({ title, children }: { title: string; children: ReactNode }) {
  useDocumentTitle(title);
  return <>{children}</>;
}

/** `/queries/new` and `/queries/:id` share one route so the post-create URL swap does not remount
 * `QueryEditorPage` and drop its unsaved buffer. */
function QueryEditorRoute() {
  const { id } = useParams<{ id: string }>();
  return (
    <RequireAuth>
      <RequirePermission
        anyOf={id === "new" ? ["create_query"] : ["view_query", "edit_query", "execute_query"]}
      >
        {/* Fallback uses the placeholder title QueryEditorPage sets on mount, else it shows the bare app name. */}
        <Suspense fallback={<RouteTitle title={id === "new" ? "New query · arcodash" : "Query · arcodash"}><PageLoader /></RouteTitle>}>
          <QueryEditorPage />
        </Suspense>
      </RequirePermission>
    </RequireAuth>
  );
}

/** Rewrites the `from` prefix of a renamed route, keeping the rest of the path and the query. */
function LegacyRedirect({ from, to }: { from: string; to: string }) {
  const location = useLocation();
  const rest = location.pathname.slice(from.length);
  return <Navigate to={`${to}${rest}${location.search}`} replace />;
}

/** `/` goes to the first nav route the caller can reach, else `/settings`. */
function IndexRedirect() {
  useDocumentTitle("arcodash");
  const { hasAny } = usePermissions();
  return <Navigate to={computeLandingPath(hasAny)} replace />;
}

// Lazily chunked routes: a mid-session deploy leaves a stale index.html importing a missing hash,
// so reload. Also used on public routes, so it must never carry error detail.
function RouteError() {
  return (
    <div className="p-6 space-y-3">
      <p className="text-sm text-destructive">Something went wrong loading this page.</p>
      <Button size="sm" onClick={() => window.location.reload()}>
        Reload
      </Button>
    </div>
  );
}

const ADMIN_ONLY = ["admin"];

/** `RequireAuth`, then `RequirePermission` when `anyOf` is given, then `RouteTitle` when `title` is. */
function guarded(element: ReactNode, anyOf?: readonly string[], title?: string) {
  const titled = title ? <RouteTitle title={title}>{element}</RouteTitle> : element;
  return (
    <RequireAuth>
      {anyOf ? <RequirePermission anyOf={anyOf}>{titled}</RequirePermission> : titled}
    </RequireAuth>
  );
}

/** A signed-in user gets the 404 inside the app shell; a signed-out visitor gets the bare page. */
function CatchAllNotFound() {
  useDocumentTitle("Page not found · arcodash");
  const { user, isLoading } = useAuth();
  if (isLoading) return <PageLoader className="h-svh" />;
  if (!user) return <NotFoundPage />;
  return (
    <AppShell>
      <NotFoundPage />
    </AppShell>
  );
}

export const router = createBrowserRouter([
  { path: "/login", element: <LoginPage />, errorElement: <RouteError /> },
  { path: "/forgot_password", element: <ForgotPasswordPage />, errorElement: <RouteError /> },
  { path: "/reset_password", element: <ResetPasswordPage />, errorElement: <RouteError /> },
  // Let an extension keep its own errorElement if it supplies one.
  ...extensions.flatMap((e) => e.routes).map((r) => ({ errorElement: <RouteError />, ...r })),
  { path: "/users/invitation/accept", element: <InviteAcceptPage />, errorElement: <RouteError /> },
  {
    path: "/public/dashboards/:token",
    element: (
      <Suspense fallback={<PageLoader />}>
        <PublicDashboardPage />
      </Suspense>
    ),
    errorElement: <RouteError />,
  },
  {
    path: "/public/embed/:token",
    element: (
      <Suspense fallback={<PageLoader />}>
        <PublicEmbedPage />
      </Suspense>
    ),
    errorElement: <RouteError />,
  },
  {
    path: "/",
    element: <AppShell />,
    errorElement: <RouteError />,
    children: [
      { index: true, element: guarded(<IndexRedirect />) },
      {
        path: "queries",
        element: guarded(
          <QueriesPage />,
          ["view_query", "create_query", "edit_query", "execute_query"],
          "Queries · arcodash",
        ),
      },
      { path: "queries/:id", element: <QueryEditorRoute /> },
      {
        path: "data_sources",
        element: guarded(<DataSourcesPage />, ["view_data_source"], "Data Sources · arcodash"),
      },
      {
        path: "data_sources/new",
        element: guarded(<DataSourceNewPage />, ["create_data_source"], "New data source · arcodash"),
      },
      { path: "data_sources/:id", element: guarded(<DataSourceDetailPage />, ["view_data_source"]) },
      {
        path: "data_sources/:id/edit",
        element: guarded(<DataSourceEditPage />, ["edit_data_source"], "Edit data source · arcodash"),
      },
      {
        path: "dashboards",
        element: guarded(
          <DashboardsPage />,
          ["view_dashboard", "create_dashboard", "edit_dashboard"],
          "Dashboards · arcodash",
        ),
      },
      {
        path: "dashboard/:slug",
        element: guarded(
          <Suspense fallback={<PageLoader />}>
            <DashboardPage />
          </Suspense>,
          ["view_dashboard", "create_dashboard", "edit_dashboard"],
        ),
      },
      { path: "monitors", element: guarded(<MonitorsPage />, ["view_monitor"], "Monitors · arcodash") },
      { path: "monitors/new", element: guarded(<MonitorPage />, ["create_monitor"]) },
      { path: "monitors/:id", element: guarded(<MonitorPage />, ["view_monitor", "edit_monitor"]) },
      { path: "alerts/*", element: <LegacyRedirect from="/alerts" to="/monitors" /> },
      { path: "data-sources/*", element: <LegacyRedirect from="/data-sources" to="/data_sources" /> },
      {
        path: "destinations",
        element: guarded(<DestinationsPage />, ["view_destinations", "manage_destinations"]),
      },
      { path: "access", element: guarded(<AccessControlPage />, ACCESS_KEYS) },
      { path: "users", element: <Navigate to="/access?tab=users" replace /> },
      { path: "groups", element: <Navigate to="/access?tab=groups" replace /> },
      { path: "organization", element: guarded(<OrganizationPage />, ADMIN_ONLY) },
      { path: "organization/ai", element: guarded(<AiSettingsPage />, ADMIN_ONLY) },
      {
        path: "organization/ai/usage",
        element: guarded(
          <Suspense fallback={<PageLoader />}>
            <AiUsagePage />
          </Suspense>,
          ADMIN_ONLY,
          "Usage · arcodash",
        ),
      },
      {
        path: "admin/status",
        element: guarded(<SystemStatusPage />, ADMIN_ONLY, "System Status · arcodash"),
      },
      { path: "admin/audit", element: guarded(<AuditLogPage />, ADMIN_ONLY, "Audit Log · arcodash") },
      { path: "settings", element: guarded(<SettingsPage />, undefined, "Settings · arcodash") },
      { path: "account", element: guarded(<AccountPage />) },
      {
        path: "settings/notifications",
        element: guarded(<NotificationSettingsPage />, undefined, "Notifications · arcodash"),
      },
      // Let an extension keep its own errorElement if it supplies one.
      ...extensions.flatMap((e) => e.layoutRoutes ?? []).map((r) => ({ errorElement: <RouteError />, ...r })),
    ],
  },
  { path: "*", element: <CatchAllNotFound /> },
]);
