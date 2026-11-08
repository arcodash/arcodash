const DOCS_BASE = "https://www.arcodash.com/docs";

/** Every anchor here must be a real heading on the target page — a `DocsLink` that 404s or
 * lands with no matching heading is worse than no link. */
export const DOCS = {
  versionHistory: "/queries-visualization/versioning-drafts",
  draftVsPublished: "/queries-visualization/versioning-drafts#draft-vs-published",
  publicLinks: "/sharing-collaboration/public-links",
  monitorEvaluation: "/alerts-monitors/data-quality-monitors#how-a-check-is-evaluated",
  dataSourceHealth: "/connecting-data/overview#connection-health",
  groupAccess: "/administration/groups-data-source-access",
  scheduledRefresh: "/queries-visualization/scheduled-refresh",
  aiUsageLimits: "/reference/limits#ai-usage",
} as const;

export type DocsTopic = keyof typeof DOCS;
export const docsUrl = (t: DocsTopic) => `${DOCS_BASE}${DOCS[t]}`;
