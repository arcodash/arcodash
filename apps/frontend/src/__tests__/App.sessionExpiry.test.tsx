import { describe, it, expect, beforeEach } from "vitest";
import { renderHook, waitFor, act } from "@testing-library/react";
import { useMutation, QueryClientProvider } from "@tanstack/react-query";
import { queryClient } from "@/App";
import { SESSION_QUERY_KEY } from "@/lib/auth";
import { ApiError } from "@/lib/api";
import { CREDENTIAL_CHECK_MUTATION_KEY } from "@/lib/credentialCheckMutationKey";
import type { SessionResponse } from "@/types/api";

const session: SessionResponse = {
  user: { id: 1, name: "Test User", email: "test@example.com", role: "admin" },
} as unknown as SessionResponse;

function wrapper({ children }: { children: React.ReactNode }) {
  return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>;
}

describe("App's MutationCache 401 handler", () => {
  beforeEach(() => {
    queryClient.setQueryData(SESSION_QUERY_KEY, session);
  });

  it("clears the cached session when a mutation rejects with a 401", async () => {
    let ownOnErrorRan = false;
    const { result } = renderHook(
      () =>
        useMutation({
          mutationFn: () => Promise.reject(new ApiError(401, "expired")),
          onError: () => {
            ownOnErrorRan = true;
          },
        }),
      { wrapper },
    );

    await act(async () => {
      result.current.mutate();
    });
    await waitFor(() => expect(result.current.isError).toBe(true));

    expect(queryClient.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.user).toBeNull();
    // The MutationCache handler runs in addition to the mutation's own onError, not instead of it.
    expect(ownOnErrorRan).toBe(true);
  });

  it("leaves the cached session alone when a mutation rejects with a non-401", async () => {
    const { result } = renderHook(
      () => useMutation({ mutationFn: () => Promise.reject(new ApiError(500, "boom")) }),
      { wrapper },
    );

    await act(async () => {
      result.current.mutate();
    });
    await waitFor(() => expect(result.current.isError).toBe(true));

    expect(queryClient.getQueryData<SessionResponse>(SESSION_QUERY_KEY)).toEqual(session);
  });

  it("leaves the cached session alone and still runs local onError for a CREDENTIAL_CHECK_MUTATION_KEY mutation's 401", async () => {
    let ownOnErrorMessage: string | undefined;
    const { result } = renderHook(
      () =>
        useMutation({
          mutationKey: [CREDENTIAL_CHECK_MUTATION_KEY, "changePassword"],
          mutationFn: () => Promise.reject(new ApiError(401, "current password is incorrect")),
          onError: (err) => {
            ownOnErrorMessage = err instanceof ApiError ? err.message : undefined;
          },
        }),
      { wrapper },
    );

    await act(async () => {
      result.current.mutate();
    });
    await waitFor(() => expect(result.current.isError).toBe(true));

    // The bad-password 401 must not read as a session expiry: no sign-out, no redirect trigger.
    expect(queryClient.getQueryData<SessionResponse>(SESSION_QUERY_KEY)).toEqual(session);
    // It must still surface locally so the user sees why the mutation failed.
    expect(ownOnErrorMessage).toBe("current password is incorrect");
  });
});
