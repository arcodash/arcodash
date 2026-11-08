// A mutating request with a session cookie but no X-CSRF-Token gets 403 (middleware/csrf.rs); CORS
// must allow x-csrf-token.
 // Rate-limit env overrides are covered by Rust integration tests; Playwright must not hammer the
 // shared login endpoint.
import { test, expect } from "@playwright/test";
import { loginAsAdmin } from "./helpers/api";

const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8080";
const FRONTEND_ORIGIN = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:3000";

test("mutating request with valid session but no X-CSRF-Token header → 403", async () => {
  const api = await loginAsAdmin();

  const res = await fetch(`${BACKEND_URL}/api/dashboards`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: api.sessionCookieHeader,
    },
    body: JSON.stringify({ name: `e2e-csrf-${Date.now()}` }),
  });

  expect(res.status).toBe(403);
  const body = await res.text();
  expect(body).toContain("CSRF token missing or mismatch");
});

test("CORS preflight: OPTIONS with Access-Control-Request-Headers x-csrf-token is allowed", async () => {
  const res = await fetch(`${BACKEND_URL}/api/dashboards`, {
    method: "OPTIONS",
    headers: {
      Origin: FRONTEND_ORIGIN,
      "Access-Control-Request-Method": "POST",
      "Access-Control-Request-Headers": "x-csrf-token",
    },
  });

  expect([200, 204]).toContain(res.status);
  const allowHeaders = (res.headers.get("access-control-allow-headers") ?? "").toLowerCase();
  expect(allowHeaders).toContain("x-csrf-token");
  expect(res.headers.get("access-control-allow-origin")).toBe(FRONTEND_ORIGIN);
});
