import type { ReactNode } from "react";
import { Navigate, useLocation, useParams } from "react-router";
import { useQuery } from "@tanstack/react-query";
import { api, ApiError } from "@/lib/api";
import { PublicLinkUnavailable } from "@/components/share/PublicLinkUnavailable";
import { PoweredBy } from "@/components/share/PoweredBy";
import { PageLoader } from "@/components/ui/loader";
import { DashboardGrid } from "@/components/dashboard/DashboardGrid";
import { toGridWidget } from "@/lib/publicEmbed";
import { exactTs, timeAgo, useNow } from "@/lib/scheduling";
import { useDocumentTitle } from "@/lib/useDocumentTitle";
import type { PublicWidget } from "@/types/api";

/** Header + body shared with PublicEmbedPage. */
export function PublicViewLayout({
  title,
  asOf,
  children,
}: {
  title: string;
  asOf: string | null;
  children: ReactNode;
}) {
  // Same relative-time convention as TileFreshnessDot, so header and tiles never state one fact in
  // two formats.
  const now = useNow(false);
  const asOfMs = asOf ? Date.parse(asOf) : null;

  return (
    <div className="min-h-dvh bg-background">
      <header className="border-b px-4 py-3 flex items-center gap-3 md:px-6">
        <h1 className="text-lg font-semibold">{title}</h1>
        <span
          className="text-xs text-muted-foreground"
          title={asOfMs != null ? exactTs(asOfMs) : undefined}
        >
          Public view{asOfMs != null ? ` · updated ${timeAgo(asOfMs, now)}` : ""}
        </span>
        <PoweredBy className="ml-auto" />
      </header>
      <main className="p-4">{children}</main>
    </div>
  );
}

export function PublicDashboardGrid({ widgets }: { widgets: PublicWidget[] }) {
  return (
    <DashboardGrid
      widgets={widgets.map(toGridWidget)}
      editMode={false}
      onDeleteWidget={() => {}}
      dashboardParams={{}}
      publicView
    />
  );
}

/** Server returns the same stripped shape as the unified embed endpoint (no raw SQL/data-source
 * config), so this renders identically to PublicEmbedPage. */
export function PublicDashboardPage() {
  const { token } = useParams<{ token: string }>();
  const location = useLocation();

  const dashQ = useQuery({
    queryKey: ["public-dashboard", token],
    queryFn: () => api.getPublicDashboard(token!),
    enabled: token != null,
  });

  useDocumentTitle(dashQ.data ? `${dashQ.data.title} · arcodash` : "Dashboard · arcodash");

  // A deployment that disables anonymous access returns 401 here. Redirect to sign-in,
  // preserving the return path so login lands back on this dashboard. Same pattern `RequireAuth`
  // uses for protected routes (see `lib/auth.tsx`).
  if (dashQ.error instanceof ApiError && dashQ.error.status === 401) {
    return <Navigate to="/login" state={{ from: location }} replace />;
  }

  if (dashQ.isLoading) {
    return <PageLoader />;
  }

  if (dashQ.isError) {
    return <PublicLinkUnavailable />;
  }

  const dashboard = dashQ.data!;
  return (
    <PublicViewLayout title={dashboard.title} asOf={dashboard.as_of}>
      <PublicDashboardGrid widgets={dashboard.widgets ?? []} />
    </PublicViewLayout>
  );
}
