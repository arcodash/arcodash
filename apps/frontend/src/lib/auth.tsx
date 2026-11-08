import { createContext, useCallback, useContext, useMemo, type ReactNode } from "react";
import { useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { Navigate, useLocation } from "react-router";
import { api } from "@/lib/api";
import { extensions } from "@/extensions";
import { PageLoader } from "@/components/ui/loader";
import { computeLandingPath, usePermissions } from "@/lib/permissions";
import { SESSION_QUERY_KEY } from "@/lib/sessionQueryKey";
import { SWITCHABLE_ORGS_QUERY_KEY } from "@/lib/switchableOrgsQueryKey";
import type { ClientConfig, Org, SessionResponse, User } from "@/types/api";

export { SESSION_QUERY_KEY };

/** Drops every cached query except the session's own when identity changes (login, logout, org switch) so nothing
 * scoped to the previous identity survives. `clear()` would destroy the session query `AuthProvider` observes. */
export function purgeNonSessionQueries(queryClient: QueryClient): void {
  queryClient.removeQueries({ predicate: (query) => query.queryKey[0] !== SESSION_QUERY_KEY[0] });
}

interface AuthContextValue {
  user: User | null;
  org: Org | null;
  clientConfig: ClientConfig | null;
  isLoading: boolean;
  login: (email: string, password: string) => Promise<User>;
  logout: () => Promise<void>;
  /** Re-fetch `/api/session` after a login that didn't go through `login()` above (LDAP). */
  refreshSession: () => Promise<void>;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient();

  const sessionQuery = useQuery({
    queryKey: SESSION_QUERY_KEY,
    queryFn: () => api.getSession(),
    retry: false,
  });

  const login = useCallback(
    async (email: string, password: string) => {
      const user = await api.login(email, password);
      purgeNonSessionQueries(queryClient);
      await queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY });
      return user;
    },
    [queryClient],
  );

  const logout = useCallback(async () => {
    try {
      await api.logout();
    } catch (err) {
      // The server session may outlive this; tear down locally regardless so the app is not stuck
      // signed in.
      console.error("Logout request failed", err);
    }
    purgeNonSessionQueries(queryClient);
    // Re-set so the session reads logged-out immediately (no loading flicker). `client_config` is
    // app-level and must survive: LoginPage shows an error card without it.
    queryClient.setQueryData<SessionResponse>(SESSION_QUERY_KEY, (old) =>
      old ? { user: null, client_config: old.client_config } : old,
    );
    // Let extensions drop session-scoped state so it does not leak to the next login in this tab.
    for (const ext of extensions) {
      try {
        ext.onLogout?.();
      } catch (err) {
        // One extension's cleanup failing must not abandon the rest of logout half-done.
        console.error("Extension onLogout failed", err);
      }
    }
  }, [queryClient]);

  const refreshSession = useCallback(async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: SESSION_QUERY_KEY }),
      queryClient.invalidateQueries({ queryKey: SWITCHABLE_ORGS_QUERY_KEY }),
    ]);
  }, [queryClient]);

  const value: AuthContextValue = useMemo(
    () => ({
      user: sessionQuery.data?.user ?? null,
      org: sessionQuery.data?.org ?? null,
      clientConfig: sessionQuery.data?.client_config ?? null,
      isLoading: sessionQuery.isLoading,
      login,
      logout,
      refreshSession,
    }),
    [
      sessionQuery.data?.user,
      sessionQuery.data?.org,
      sessionQuery.data?.client_config,
      sessionQuery.isLoading,
      login,
      logout,
      refreshSession,
    ],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth() {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used within an AuthProvider");
  return ctx;
}

export function RequireAuth({ children }: { children: ReactNode }) {
  const { user, isLoading } = useAuth();
  const location = useLocation();

  if (isLoading) return <PageLoader className="h-svh" />;
  if (!user) return <Navigate to="/login" state={{ from: location }} replace />;
  return <>{children}</>;
}

/** `RequireAuth` plus at least one of `anyOf`; silently redirects to the landing page, never a
 * dead-end error. */
export function RequirePermission({ anyOf, children }: { anyOf: readonly string[]; children: ReactNode }) {
  const { hasAny, isLoading } = usePermissions();

  if (isLoading) return <PageLoader className="h-svh" />;
  if (!hasAny(anyOf)) return <Navigate to={computeLandingPath(hasAny)} replace />;
  return <>{children}</>;
}
