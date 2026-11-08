/** Shared `/api/session` query key — split into its own module so `lib/auth.tsx` and
 * `lib/permissions.ts` can both depend on it without importing each other. */
export const SESSION_QUERY_KEY = ["session"] as const;
