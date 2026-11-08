/** Shared react-query `QueryClient` plus the 401 session-clear handler; separate from `App.tsx` so
 * `lib/api.ts` can call it directly without an import cycle (covering raw `api.*` calls outside
 * react-query). */
import { MutationCache, QueryCache, QueryClient } from "@tanstack/react-query";
import { ApiError } from "@/lib/api";
import { CREDENTIAL_CHECK_MUTATION_KEY } from "@/lib/credentialCheckMutationKey";
import { SESSION_QUERY_KEY } from "@/lib/sessionQueryKey";
import type { SessionResponse } from "@/types/api";

/** Drops the cached session's `user` on a real 401; both react-query and raw-fetch (`apiFetch`)
 * paths funnel here. */
export function notifySessionExpired() {
  queryClient.setQueryData<SessionResponse>(SESSION_QUERY_KEY, (old) => (old ? { ...old, user: null } : old));
}

// A 401 from anything but the session probe means the session expired; drop the cached user so the
// router redirects. `login()` calls the API directly, so it never reaches this.
export function handleExpiredSession(err: unknown, queryKey?: readonly unknown[]) {
  if (queryKey?.[0] === SESSION_QUERY_KEY[0]) return;
  if (err instanceof ApiError && err.status === 401) notifySessionExpired();
}

export const queryClient = new QueryClient({
  queryCache: new QueryCache({ onError: (err, query) => handleExpiredSession(err, query.queryKey) }),
  // Runs in addition to each mutation's own onError. Mutations tagged CREDENTIAL_CHECK_MUTATION_KEY
  // are skipped: their 401 means wrong credentials, not an expired session.
  mutationCache: new MutationCache({
    onError: (err, _variables, _onMutateResult, mutation) => {
      if (mutation.options.mutationKey?.[0] === CREDENTIAL_CHECK_MUTATION_KEY) return;
      handleExpiredSession(err);
    },
  }),
  defaultOptions: {
    queries: {
      retry: false,
      refetchOnWindowFocus: false,
      // Without this every navigation refetches data fetched moments ago; 30s absorbs normal
      // back-and-forth browsing.
      staleTime: 30_000,
    },
  },
});
