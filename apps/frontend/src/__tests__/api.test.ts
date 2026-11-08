/**
 * Tests for the api.ts fetch client.
 * getCsrfToken and apiFetch are not exported, so we test them through the public `api` object.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { api, ApiError, ConflictError } from "@/lib/api";

const BASE = "http://localhost:8080";

// Helper: set a document.cookie value for the duration of a test.
function setCookie(name: string, value: string) {
  document.cookie = `${name}=${value}; path=/`;
}
function clearCookie(name: string) {
  document.cookie = `${name}=; max-age=0; path=/`;
}

describe("getCsrfToken (via apiFetch internals)", () => {
  beforeEach(() => clearCookie("csrf_token"));

  it("reads csrf_token cookie and sends it on POST", async () => {
    setCookie("csrf_token", "test-csrf-abc");

    let capturedHeader: string | null = null;
    server.use(
      http.post(`${BASE}/api/logout`, ({ request }) => {
        capturedHeader = request.headers.get("X-CSRF-Token");
        return HttpResponse.json({});
      }),
    );

    await api.logout();
    expect(capturedHeader).toBe("test-csrf-abc");
  });

  it("does NOT send X-CSRF-Token on GET", async () => {
    setCookie("csrf_token", "should-not-appear");

    let capturedHeader: string | null | undefined = undefined;
    server.use(
      http.get(`${BASE}/api/session`, ({ request }) => {
        capturedHeader = request.headers.get("X-CSRF-Token");
        return HttpResponse.json({ user: null });
      }),
    );

    await api.getSession();
    expect(capturedHeader).toBeNull();
  });
});

describe("apiFetch mutation methods send X-CSRF-Token", () => {
  beforeEach(() => {
    clearCookie("csrf_token");
    setCookie("csrf_token", "csrf-xyz");
  });
  afterEach(() => clearCookie("csrf_token"));

  it("sends header on DELETE", async () => {
    let capturedHeader: string | null = null;
    server.use(
      http.delete(`${BASE}/api/queries/1/favorite`, ({ request }) => {
        capturedHeader = request.headers.get("X-CSRF-Token");
        return new HttpResponse(null, { status: 204 });
      }),
    );

    await api.unstarQuery(1);
    expect(capturedHeader).toBe("csrf-xyz");
  });

  it("sends header on PATCH-like POST", async () => {
    let capturedHeader: string | null = null;
    server.use(
      http.post(`${BASE}/api/groups`, ({ request }) => {
        capturedHeader = request.headers.get("X-CSRF-Token");
        return HttpResponse.json({ id: 99, name: "g", type: "regular", permissions: [] });
      }),
    );

    await api.createGroup({ name: "g" });
    expect(capturedHeader).toBe("csrf-xyz");
  });
});

describe("roles API", () => {
  it("listRoles() parses the role list", async () => {
    server.use(
      http.get(`${BASE}/api/roles`, () =>
        HttpResponse.json([
          {
            id: 1,
            name: "Admin",
            type: "builtin",
            permissions: ["admin"],
            group_count: 2,
            created_at: "2025-01-01T00:00:00Z",
            updated_at: "2025-01-01T00:00:00Z",
          },
        ]),
      ),
    );

    const roles = await api.listRoles();
    expect(roles).toEqual([
      {
        id: 1,
        name: "Admin",
        type: "builtin",
        permissions: ["admin"],
        group_count: 2,
        created_at: "2025-01-01T00:00:00Z",
        updated_at: "2025-01-01T00:00:00Z",
      },
    ]);
  });
});

describe("sharing ACL (grant ladder + group grants)", () => {
  it("grantAcl round-trips a group grant (grantee_group_id, no grantee_id)", async () => {
    let body: unknown = null;
    server.use(
      http.post(`${BASE}/api/dashboards/5/acl`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({
          id: 20,
          object_type: "dashboard",
          object_id: 5,
          access_type: "editor",
          grantor_id: 1,
          grantee_id: null,
          grantee_group_id: 3,
          created_at: "2026-01-01T00:00:00Z",
        });
      }),
    );

    const result = await api.grantAcl("dashboard", 5, { grantee_group_id: 3, access_type: "editor" });

    expect(body).toEqual({ grantee_group_id: 3, access_type: "editor" });
    expect(result).toMatchObject({ grantee_id: null, grantee_group_id: 3, access_type: "editor" });
  });
});

describe("apiFetch error handling", () => {
  it("throws ApiError on 401", async () => {
    server.use(
      http.get(`${BASE}/api/session`, () =>
        HttpResponse.json({ message: "Unauthorized" }, { status: 401 }),
      ),
    );

    await expect(api.getSession()).rejects.toMatchObject({
      name: "ApiError",
      status: 401,
      message: "Unauthorized",
    });
  });

  it("throws ApiError on 500, falls back to statusText when body is not JSON", async () => {
    server.use(
      http.get(`${BASE}/api/session`, () =>
        new HttpResponse("Internal Server Error", {
          status: 500,
          headers: { "Content-Type": "text/plain" },
        }),
      ),
    );

    await expect(api.getSession()).rejects.toMatchObject({
      name: "ApiError",
      status: 500,
    });
  });

  it("returns undefined on 204", async () => {
    server.use(
      http.delete(`${BASE}/api/queries/1/favorite`, () =>
        new HttpResponse(null, { status: 204 }),
      ),
    );

    const result = await api.unstarQuery(1);
    expect(result).toBeUndefined();
  });
});

describe("apiFetch default timeout", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("aborts the underlying fetch and rejects with 408 when no caller signal is given", async () => {
    vi.useFakeTimers();
    let capturedSignal: AbortSignal | undefined;
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn((_input: RequestInfo | URL, init?: RequestInit) => {
      capturedSignal = init?.signal ?? undefined;
      return new Promise<Response>(() => {});
    }) as unknown as typeof fetch;

    try {
      const promise = api.getSession();
      const assertion = expect(promise).rejects.toMatchObject({ name: "ApiError", status: 408 });
      await vi.advanceTimersByTimeAsync(60_000);
      await assertion;
      expect(capturedSignal?.aborted).toBe(true);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it("does not time out a request made with a caller-supplied signal", async () => {
    vi.useFakeTimers();
    const originalFetch = globalThis.fetch;
    globalThis.fetch = vi.fn(() => new Promise<Response>(() => {})) as unknown as typeof fetch;

    try {
      const controller = new AbortController();
      const promise = api.suggestVisualization({ columns: [], row_count: 0 }, controller.signal);
      let settled = false;
      promise.then(
        () => (settled = true),
        () => (settled = true),
      );
      await vi.advanceTimersByTimeAsync(60_000);
      expect(settled).toBe(false);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});

describe("ConflictError parsing", () => {
  it("parses a 409 conflict body into a ConflictError carrying working_version", async () => {
    server.use(
      http.post(`${BASE}/api/queries/1/publish`, () =>
        HttpResponse.json(
          { message: "conflict", error_code: "conflict", working_version: 7 },
          { status: 409 },
        ),
      ),
    );

    await expect(api.publishQuery(1, 3)).rejects.toBeInstanceOf(ConflictError);
    try {
      await api.publishQuery(1, 3);
      throw new Error("expected rejection");
    } catch (err) {
      expect(err).toBeInstanceOf(ConflictError);
      expect((err as ConflictError).workingVersion).toBe(7);
      expect((err as ConflictError).status).toBe(409);
    }
  });

  it("falls back to a plain ApiError on a 409 without the conflict shape", async () => {
    server.use(
      http.post(`${BASE}/api/queries/1/discard`, () =>
        HttpResponse.json({ message: "some other 409" }, { status: 409 }),
      ),
    );

    await expect(api.discardQueryChanges(1, 3)).rejects.toBeInstanceOf(ApiError);
    await expect(api.discardQueryChanges(1, 3)).rejects.not.toBeInstanceOf(ConflictError);
  });
});
