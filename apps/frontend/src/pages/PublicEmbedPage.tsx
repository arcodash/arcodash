import { useEffect } from "react";
import { Navigate, useLocation, useParams } from "react-router";
import { useQuery } from "@tanstack/react-query";
import { api, ApiError } from "@/lib/api";
import { ErrorBoundary } from "@/components/ErrorBoundary";
import { PublicLinkUnavailable } from "@/components/share/PublicLinkUnavailable";
import { PageLoader } from "@/components/ui/loader";
import { getViz, toVizData } from "@/components/visualizations/registry";
import { useDocumentTitle } from "@/lib/useDocumentTitle";
import type { PublicEmbedResponse, ResultData } from "@/types/api";
import { PublicDashboardGrid, PublicViewLayout } from "@/pages/PublicDashboardPage";

export function PublicEmbedPage() {
  const { token } = useParams<{ token: string }>();
  const location = useLocation();

  const embedQ = useQuery({
    queryKey: ["public-embed", token],
    queryFn: () => api.getPublicEmbed(token!),
    enabled: token != null,
  });

  useDocumentTitle(embedQ.data ? `${embedQ.data.title} · arcodash` : "Shared view · arcodash");

  // No other page sets a robots meta tag, so remove it outright on cleanup; rewriting it would
  // leave an index directive for the next client-side page.
  useEffect(() => {
    const meta = document.createElement("meta");
    meta.setAttribute("name", "robots");
    meta.setAttribute("content", "noindex, nofollow");
    document.head.appendChild(meta);
    return () => {
      meta.remove();
    };
  }, []);

  // A deployment that disables anonymous access returns 401 here.
  if (embedQ.error instanceof ApiError && embedQ.error.status === 401) {
    return <Navigate to="/login" state={{ from: location }} replace />;
  }

  if (embedQ.isLoading) {
    return <PageLoader />;
  }

  if (embedQ.isError || !embedQ.data) {
    // The backend returns a uniform 404 for every failure reason
    // (revoked/expired/bad-token/org-paused) to avoid leaking it; copy is hardcoded.
    return <PublicLinkUnavailable />;
  }

  const response = embedQ.data;
  return (
    <PublicViewLayout title={response.title} asOf={response.as_of}>
      {response.object_type === "dashboard" ? (
        <PublicDashboardGrid widgets={response.widgets} />
      ) : (
        <SingleVisualization viz={response.visualization} />
      )}
    </PublicViewLayout>
  );
}

function SingleVisualization({ viz }: { viz: PublicEmbedResponse["visualization"] }) {
  if (!viz) {
    return <p className="text-muted-foreground">Nothing to display.</p>;
  }
  const def = getViz(viz.type);
  if (!def) {
    return (
      <div className="p-4 text-sm text-muted-foreground">
        Unknown visualization type &ldquo;{viz.type}&rdquo;.
      </div>
    );
  }
  return (
    <ErrorBoundary>
      {/* A fixed height (not min-height) gives a chart's height="100%" ResponsiveContainer a real ancestor; full-width up to a cap so one tile doesn't sit at intrinsic size in the corner. */}
      <div className="mx-auto flex h-[70vh] min-h-[24rem] w-full max-w-5xl flex-col overflow-auto rounded-md border border-border bg-card p-4 shadow-sm">
        <def.Renderer data={toVizData(viz.data as ResultData | null)} options={viz.options} title={viz.name} />
      </div>
    </ErrorBoundary>
  );
}
