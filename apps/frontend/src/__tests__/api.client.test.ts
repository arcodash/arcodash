import { describe, it, expect } from "vitest";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { api } from "@/lib/api";

const BASE = "http://localhost:8080";

describe("listQueries query string", () => {
  it("defaults to include_archived=false and omits q", async () => {
    let captured = "";
    server.use(
      http.get(`${BASE}/api/queries`, ({ request }) => {
        captured = new URL(request.url).search;
        return HttpResponse.json([]);
      }),
    );

    await api.listQueries();
    expect(captured).toBe("?include_archived=false");
  });

  it("sets include_archived=true when requested", async () => {
    let captured = "";
    server.use(
      http.get(`${BASE}/api/queries`, ({ request }) => {
        captured = new URL(request.url).search;
        return HttpResponse.json([]);
      }),
    );

    await api.listQueries(true);
    expect(captured).toBe("?include_archived=true");
  });

  it("appends and url-encodes q", async () => {
    let captured = "";
    server.use(
      http.get(`${BASE}/api/queries`, ({ request }) => {
        const url = new URL(request.url);
        captured = url.search;
        return HttpResponse.json([]);
      }),
    );

    await api.listQueries(false, "a b&c");
    expect(captured).toBe("?include_archived=false&q=a%20b%26c");
  });

  it("combines include_archived=true and q", async () => {
    let captured = "";
    server.use(
      http.get(`${BASE}/api/queries`, ({ request }) => {
        captured = new URL(request.url).search;
        return HttpResponse.json([]);
      }),
    );

    await api.listQueries(true, "sales");
    expect(captured).toBe("?include_archived=true&q=sales");
  });
});

describe("listEvents query string", () => {
  it("sends no query string when no params are given", async () => {
    let capturedUrl = "";
    server.use(
      http.get(`${BASE}/api/events`, ({ request }) => {
        capturedUrl = request.url;
        return HttpResponse.json([]);
      }),
    );

    await api.listEvents();
    expect(new URL(capturedUrl).search).toBe("");
  });

  it("builds page, page_size and action params", async () => {
    let params: URLSearchParams = new URLSearchParams();
    server.use(
      http.get(`${BASE}/api/events`, ({ request }) => {
        params = new URL(request.url).searchParams;
        return HttpResponse.json([]);
      }),
    );

    await api.listEvents({ page: 2, page_size: 50, action: "login" });
    expect(params.get("page")).toBe("2");
    expect(params.get("page_size")).toBe("50");
    expect(params.get("action")).toBe("login");
  });

  it("sends action alone", async () => {
    let search = "";
    server.use(
      http.get(`${BASE}/api/events`, ({ request }) => {
        search = new URL(request.url).search;
        return HttpResponse.json([]);
      }),
    );

    await api.listEvents({ action: "query.execute" });
    expect(search).toBe("?action=query.execute");
  });

  it("drops undefined params", async () => {
    let params: URLSearchParams = new URLSearchParams();
    server.use(
      http.get(`${BASE}/api/events`, ({ request }) => {
        params = new URL(request.url).searchParams;
        return HttpResponse.json([]);
      }),
    );

    await api.listEvents({ page: 3, action: undefined, user_id: undefined });
    expect(params.get("page")).toBe("3");
    expect(params.has("action")).toBe(false);
    expect(params.has("user_id")).toBe(false);
    expect(params.has("page_size")).toBe(false);
  });
});

describe("revertQuery body shape", () => {
  it("sends { change_id } exactly as passed", async () => {
    let body: unknown = null;
    server.use(
      http.post(`${BASE}/api/queries/7/revert`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({ id: 7 });
      }),
    );

    await api.revertQuery(7, { change_id: 42 });
    expect(body).toEqual({ change_id: 42 });
  });

  it("sends { version } exactly as passed, with no change_id cross-contamination", async () => {
    let body: Record<string, unknown> = {};
    server.use(
      http.post(`${BASE}/api/queries/7/revert`, async ({ request }) => {
        body = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({ id: 7 });
      }),
    );

    await api.revertQuery(7, { version: 3 });
    expect(body).toEqual({ version: 3 });
    expect(body).not.toHaveProperty("change_id");
  });
});

describe("query ACL", () => {
  it("grantAcl POSTs to /acl with { grantee_id, access_type } and returns the permission", async () => {
    let body: unknown = null;
    server.use(
      http.post(`${BASE}/api/queries/5/acl`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({ id: 11, grantee_id: 9, access_type: "editor" });
      }),
    );

    const result = await api.grantAcl("query", 5, { grantee_id: 9, access_type: "editor" });
    expect(body).toEqual({ grantee_id: 9, access_type: "editor" });
    expect(result).toEqual({ id: 11, grantee_id: 9, access_type: "editor" });
  });

  it("revokeAcl issues DELETE to /queries/:id/acl/:permId", async () => {
    let method = "";
    server.use(
      http.delete(`${BASE}/api/queries/5/acl/11`, ({ request }) => {
        method = request.method;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    const result = await api.revokeAcl("query", 5, 11);
    expect(method).toBe("DELETE");
    expect(result).toBeUndefined();
  });
});

describe("executeQuery", () => {
  it("POSTs { parameters, max_age } and parses the { job } envelope", async () => {
    let body: unknown = null;
    server.use(
      http.post(`${BASE}/api/queries/4/results`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({ job: { id: "job-1", status: 1 } });
      }),
    );

    const result = await api.executeQuery(4, { parameters: { x: 1 }, max_age: 60 });
    expect(body).toEqual({ parameters: { x: 1 }, max_age: 60 });
    expect(result).toEqual({ job: { id: "job-1", status: 1 } });
  });

  it("sends {} when no body is provided", async () => {
    let body: unknown = "unset";
    server.use(
      http.post(`${BASE}/api/queries/4/results`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({ job: { id: "job-2" } });
      }),
    );

    await api.executeQuery(4);
    expect(body).toEqual({});
  });

  it("forwards a partial body (parameters only)", async () => {
    let body: unknown = null;
    server.use(
      http.post(`${BASE}/api/queries/4/results`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json({ job: { id: "job-3" } });
      }),
    );

    await api.executeQuery(4, { parameters: { a: "b" } });
    expect(body).toEqual({ parameters: { a: "b" } });
  });

  it("parses the { query_result } envelope (cached result)", async () => {
    server.use(
      http.post(`${BASE}/api/queries/4/results`, () =>
        HttpResponse.json({ query_result: { id: 99, data: { rows: [] } } }),
      ),
    );

    const result = await api.executeQuery(4, { max_age: 3600 });
    expect(result).toEqual({ query_result: { id: 99, data: { rows: [] } } });
  });

  it("throws ApiError on 422", async () => {
    server.use(
      http.post(`${BASE}/api/queries/4/results`, () =>
        HttpResponse.json({ message: "Invalid parameters" }, { status: 422 }),
      ),
    );

    await expect(api.executeQuery(4, { parameters: {} })).rejects.toMatchObject({
      name: "ApiError",
      status: 422,
      message: "Invalid parameters",
    });
  });
});
