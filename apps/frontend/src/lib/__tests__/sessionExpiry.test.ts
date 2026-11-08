/**
 * `apiFetch`'s own 401 branch must clear the cached session for EVERY caller, not just ones
 * that go through react-query — a raw `api.*` call made outside a query/mutation (a layout
 * autosave, a test-connection poll) would otherwise never reach the session-expiry handler.
 */
import { describe, it, expect, beforeEach } from "vitest";
import { http, HttpResponse } from "msw";
import { server } from "../../__tests__/msw-server";
import { api, ApiError } from "../api";
import { queryClient } from "../sessionExpiry";
import { SESSION_QUERY_KEY } from "../sessionQueryKey";
import type { SessionResponse } from "@/types/api";

const BASE = "http://localhost:8080";

const session: SessionResponse = {
  user: { id: 1, name: "Test User", email: "test@example.com" },
} as unknown as SessionResponse;

describe("apiFetch 401 -> session clear (raw calls outside react-query)", () => {
  beforeEach(() => {
    queryClient.setQueryData(SESSION_QUERY_KEY, session);
  });

  it("clears the cached session's user when a raw api.* call 401s", async () => {
    server.use(
      http.put(`${BASE}/api/dashboards/:slug/layout`, () =>
        HttpResponse.json({ message: "session expired" }, { status: 401 }),
      ),
    );

    await expect(api.saveDashboardLayout("sales", [])).rejects.toBeInstanceOf(ApiError);
    expect(queryClient.getQueryData<SessionResponse>(SESSION_QUERY_KEY)?.user).toBeNull();
  });

  it("leaves the cached session alone on a non-401 error from a raw call", async () => {
    server.use(
      http.put(`${BASE}/api/dashboards/:slug/layout`, () =>
        HttpResponse.json({ message: "boom" }, { status: 500 }),
      ),
    );

    await expect(api.saveDashboardLayout("sales", [])).rejects.toBeInstanceOf(ApiError);
    expect(queryClient.getQueryData<SessionResponse>(SESSION_QUERY_KEY)).toEqual(session);
  });

  it("does not clear the session on a 401 from the session probe itself", async () => {
    server.use(
      http.get(`${BASE}/api/session`, () =>
        HttpResponse.json({ message: "unauthorized" }, { status: 401 }),
      ),
    );

    await expect(api.getSession()).rejects.toBeInstanceOf(ApiError);
    expect(queryClient.getQueryData<SessionResponse>(SESSION_QUERY_KEY)).toEqual(session);
  });

  it("does not clear the session on a wrong-current-password 401 from changePassword", async () => {
    server.use(
      http.post(`${BASE}/api/users/me/password`, () =>
        HttpResponse.json({ message: "current password is incorrect" }, { status: 401 }),
      ),
    );

    await expect(
      api.changePassword({ current_password: "wrong", new_password: "new" }),
    ).rejects.toBeInstanceOf(ApiError);
    expect(queryClient.getQueryData<SessionResponse>(SESSION_QUERY_KEY)).toEqual(session);
  });
});
