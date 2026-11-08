import { describe, it, expect, afterEach } from "vitest";
import { http, HttpResponse } from "msw";
import { server } from "../__tests__/msw-server";
import { api, ApiError } from "./api";

const BASE = "http://localhost:8080";

describe("api.getSession", () => {
  it("returns the session on 200", async () => {
    const res = await api.getSession();
    expect(res.user?.email).toBe("test@example.com");
  });

  it("throws ApiError on 401", async () => {
    server.use(
      http.get(`${BASE}/api/session`, () =>
        HttpResponse.json({ message: "unauthorized" }, { status: 401 }),
      ),
    );
    await expect(api.getSession()).rejects.toBeInstanceOf(ApiError);
  });
});

describe("api.getSession — window.__SESSION_PRELOAD__ (index.html's inline preload)", () => {
  afterEach(() => {
    delete window.__SESSION_PRELOAD__;
  });

  it("consumes a 200 preload with zero extra requests, then deletes the global", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/session`, () => {
        calls += 1;
        return HttpResponse.json({ user: { id: 1, name: "Test", email: "test@example.com" } });
      }),
    );

    window.__SESSION_PRELOAD__ = fetch(`${BASE}/api/session`, { credentials: "include" });
    const res = await api.getSession();

    expect(res.user?.email).toBe("test@example.com");
    expect(calls).toBe(1);
    expect(window.__SESSION_PRELOAD__).toBeUndefined();
  });

  it("rejects a 401 preload as ApiError with no fallback request", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/session`, () => {
        calls += 1;
        return HttpResponse.json({ message: "unauthorized" }, { status: 401 });
      }),
    );

    window.__SESSION_PRELOAD__ = fetch(`${BASE}/api/session`, { credentials: "include" });
    await expect(api.getSession()).rejects.toBeInstanceOf(ApiError);
    expect(calls).toBe(1);
  });

  it("falls back to exactly one real request when the preload itself rejects", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/session`, () => {
        calls += 1;
        return HttpResponse.json({ user: null });
      }),
    );

    window.__SESSION_PRELOAD__ = Promise.reject(new TypeError("network error"));
    const res = await api.getSession();

    expect(res.user).toBeNull();
    expect(calls).toBe(1);
  });

  it("issues a normal request on a second call, once the preload is already consumed", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/session`, () => {
        calls += 1;
        return HttpResponse.json({ user: null });
      }),
    );

    window.__SESSION_PRELOAD__ = fetch(`${BASE}/api/session`, { credentials: "include" });
    await api.getSession();
    await api.getSession();

    expect(calls).toBe(2);
  });
});

describe("api.login", () => {
  it("posts credentials and returns user", async () => {
    const user = await api.login("test@example.com", "password");
    expect(user.id).toBe(1);
  });
});

describe("api.listDataSources", () => {
  it("returns an empty list envelope by default", async () => {
    const res = await api.listDataSources();
    expect(Array.isArray(res.data_sources)).toBe(true);
    expect(res.data_sources).toHaveLength(0);
    expect(res.can_create).toBe(true);
  });
});

describe("api.askStream", () => {
  it("delivers profile_refreshing followed by final from one chunk", async () => {
    server.use(
      http.post(`${BASE}/api/queries/ask`, () =>
        new HttpResponse(
          'event: profile_refreshing\ndata: {"data_source_id":7}\n\nevent: final\ndata: {"sql":"s","attempts":0}\n\n',
          { headers: { "content-type": "text/event-stream" } },
        ),
      ),
    );
    const types: string[] = [];
    await api.askStream({ data_source_id: 7, question: "q" }, (e) => types.push(e.type));
    expect(types).toEqual(["profile_refreshing", "final"]);
  });
});
