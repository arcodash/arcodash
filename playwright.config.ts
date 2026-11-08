import { readFileSync } from "node:fs";
import { defineConfig, devices } from "@playwright/test";

const BASE_URL = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:3000";
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8080";
const TEST_DATABASE_URL =
  process.env.TEST_DATABASE_URL ?? "postgres://arcodash:arcodash@localhost:5433/arcodash_test";
const BACKEND_PACKAGE = process.env.BACKEND_PACKAGE ?? "arcodash-backend";
const WORKER_PACKAGE = process.env.WORKER_PACKAGE ?? "arcodash-worker";
const WORKER_BIN = process.env.WORKER_BIN ?? WORKER_PACKAGE;
const CARGO_MANIFEST_PATH = process.env.CARGO_MANIFEST_PATH ?? "Cargo.toml";

// Optional dotenv file (KEY=VALUE lines) merged over the backend's env.
function loadEnvFile(path: string | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  if (!path) return out;
  for (const line of readFileSync(path, "utf8").split("\n")) {
    const m = line.match(/^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$/);
    if (m) out[m[1]] = m[2].replace(/^(['"])(.*)\1$/, "$2");
  }
  return out;
}
const EXTRA_ENV = loadEnvFile(process.env.E2E_EXTRA_ENV_FILE);

// Dummy test secrets, only valid for the ephemeral E2E stack (docker-compose.testing.yml's
// postgres/valkey on 5433/6380). Shared by the backend and worker webServer entries below so both
// processes agree on DB/queue/crypto config.
const TEST_ENV = {
  DATABASE_URL: TEST_DATABASE_URL,
  VALKEY_URL: "redis://localhost:6380",
  // Pinned: the worker readiness wait below needs its info-level "worker started" log.
  RUST_LOG: "info",
  // The default login rate limit (10/min/IP, see middleware/rate_limit.rs) is sized for real users;
  // specs all log in from the same loopback IP, so raise it to avoid 429s.
  LOGIN_RATE_LIMIT_PER_MIN: "1000",
  // /forgot_password, /reset_password and /users/invitation/accept share this quota (default
  // 5/min/IP); raised for the same reason as above.
  ACCOUNT_RECOVERY_RATE_LIMIT_PER_MIN: "1000",
  // Specs invite users as the one shared admin (default 10/min per user).
  USER_INVITE_RATE_LIMIT_PER_MIN: "1000",
  // Blank rather than unset: dotenvy never overrides an existing var, so a developer's .env with
  // real AI keys cannot leak into the run.
  GEMINI_API_KEY: "",
  OPENAI_API_KEY: "",
  ANTHROPIC_API_KEY: "",
  ENCRYPTION_KEY: "a".repeat(64), // 64 hex chars — Config::from_env requires exactly this
  COOKIE_SECRET: "e2e-test-cookie-secret-not-for-production-use",
  // Mailpit (docker-compose.testing.yml, http :8026 / smtp :1026) so email specs exercise the real
  // SMTP send path. SMTP_TLS=none because Mailpit speaks plaintext SMTP only.
  SMTP_HOST: "localhost",
  SMTP_PORT: "1026",
  SMTP_FROM: "test@arcodash.local",
  SMTP_TLS: "none",
  // The SSRF guard blocks connectors targeting private/internal addresses by default; the
  // localhost target-postgres test data source (:5434) is one. Same as scripts/test-connectors.sh.
  ARCODASH_ALLOW_PRIVATE_CONNECTORS: "1",
  // Configured but unreachable on purpose: lets security.spec.ts trigger a real pre-auth failure
  // (LDAP connection refused) on `POST /api/ldap/login` instead of the 503 "not configured".
  LDAP_URL: "ldap://127.0.0.1:1",
  LDAP_SEARCH_BASE_DN: "dc=example,dc=com",
  // fake-gcs-server (docker-compose.testing.yml's `gcs` service, host port 4444 -> container
  // 4443). Required at startup (CSV/SQLite upload data sources).
  UPLOAD_BUCKET: "arcodash-uploads-test",
  STORAGE_EMULATOR_HOST: "http://localhost:4444",
  // The monitor/escalation tick defaults to 300s; 5s (floored at 1s in
  // apps/worker/src/scheduler/mod.rs) keeps escalation timing observable in tests.
  MONITOR_TICK_SECS: "5",
  // Boot-time smoke check of the scheduled-task path; its behavior is covered by the Rust tests in
  // apps/worker/src/scheduler/escalations.rs and apps/worker/src/handlers/digest_fanout.rs.
  CONVEYOR_SCHEDULER_ENABLED: "1",
  BASE_URL,
  // First-run bootstrap (apps/backend/src/bootstrap.rs): creates this admin if absent.
  // Matches tests/e2e/helpers/{auth,api}.ts defaults.
  ADMIN_EMAIL: "admin@example.com",
  ADMIN_PASSWORD: "adminpassword",
};

// Reusing servers already listening on the test ports is opt-in locally: a stale backend, a missing
// admin user or a foreign worker draining the queue would corrupt the run.
const reuseExistingServer = !process.env.CI && process.env.E2E_REUSE_SERVER === "1";

export default defineConfig({
  testDir: "./tests/e2e",
  globalSetup: "./tests/e2e/global-setup.ts",
  fullyParallel: false, // E2E shares one stack; avoid race conditions
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: [["list"], ["html", { open: "never" }]],

  use: {
    baseURL: BASE_URL,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
  },

  // Started in array order (each fully up before the next begins). Worker and frontend assume the
  // backend's DB is already migrated, which CI and scripts/test.sh do beforehand.
  // E2E_REUSE_SERVER=1 reuses a developer's own `cargo run`/`pnpm dev`.
  webServer: [
    {
      name: "backend",
      command: `cargo run --manifest-path ${CARGO_MANIFEST_PATH} --package ${BACKEND_PACKAGE}`,
      url: `${BACKEND_URL}/health`,
      env: {
        ...TEST_ENV,
        APP_PORT: "8080",
        ...EXTRA_ENV,
      },
      reuseExistingServer,
      // First run compiles the debug binary.
      timeout: 300_000,
    },
    {
      name: "worker",
      command: `cargo run --manifest-path ${CARGO_MANIFEST_PATH} --package ${WORKER_PACKAGE} --bin ${WORKER_BIN}`,
      // No HTTP surface to poll — the worker only consumes a valkey queue — so readiness is a
      // log line instead of a url/port.
      wait: { stdout: /worker started/ },
      env: {
        ...TEST_ENV,
        // The worker binds a health port from APP_PORT; pin it so it cannot collide with the
        // backend's 8080 via the ambient environment or .env.
        APP_PORT: "8090",
      },
      reuseExistingServer,
      timeout: 180_000,
    },
    {
      name: "frontend",
      command: "pnpm --filter @arcodash/frontend dev",
      url: BASE_URL,
      env: {
        VITE_API_BASE_URL: BACKEND_URL,
        // Optional UI extension entry (see vite.config.ts's '@/extensions' alias), relative to
        // apps/frontend.
        ...(process.env.ARCODASH_EXT ? { ARCODASH_EXT: process.env.ARCODASH_EXT } : {}),
      },
      reuseExistingServer,
      timeout: 60_000,
    },
  ],

  projects: [
    // Chromium only for CI speed. Add webkit/firefox locally if needed.
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
});
