/** Shared `/api/orgs` (switcher list) query key — split into its own module so `lib/auth.tsx`
 * and an extension's org-switcher components can both depend on it without importing
 * each other. */
export const SWITCHABLE_ORGS_QUERY_KEY = ["switchable-orgs"] as const;
