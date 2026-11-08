// Requires: backend :8080, frontend :3000, worker (redis :6380), target postgres :5434, mailpit http :8026 / smtp :1026.
import { test, expect, type BrowserContext } from "@playwright/test";
import { loginAsAnyLanding } from "./helpers/auth";
import { loginAsAdmin, E2EApiClient, TARGET_DS_OPTIONS, executeAndWait } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { seedBuiltinRoleUser, type SeededUser } from "./helpers/permissions";
import { enqueueEvaluateMonitor, lpush } from "./helpers/valkey";
import { startStub, useStubProvider, STUB_SUMMARY } from "./helpers/ai-stub";
import { MAILPIT_URL, type MailpitMessage, fetchMailpitMessage } from "./helpers/mailpit";

// Pinned so the browser-detected timezone is deterministic across machines and CI.
const CONTEXT_TIMEZONE = "UTC";

// Scoped to the recipient: the digest subject is generic and the shared Mailpit holds mail from
// every spec.
async function digestMessagesFor(email: string): Promise<MailpitMessage[]> {
  const query = `to:"${email}" subject:"arcodash digest"`;
  const res = await fetch(`${MAILPIT_URL}/api/v1/search?query=${encodeURIComponent(query)}&limit=50`);
  if (!res.ok) return [];
  return ((await res.json()) as { messages: MailpitMessage[] }).messages ?? [];
}

async function pollDigestMailpitMessage(email: string, timeoutMs: number): Promise<MailpitMessage> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const [found] = await digestMessagesFor(email);
    if (found) return found;
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`No mailpit digest message for ${email} within ${timeoutMs}ms`);
}

// Enqueues a SendDigest and resolves once the worker finalizes its mail_deliveries row, the only
// proof the job ran when it sends nothing.
async function sendDigestAndWait(orgId: number, userId: number): Promise<void> {
  const [{ id }] = await queryRows<{ id: number }>(
    "INSERT INTO mail_deliveries (org_id, task_type, entity_id, status) VALUES ($1, 'send_digest', $2, 'pending') RETURNING id",
    [orgId, userId],
  );
  await lpush(
    "arcodash:queue:default",
    JSON.stringify({
      type: "SendDigest",
      org_id: Number(orgId),
      user_id: Number(userId),
      delivery_id: Number(id),
    }),
  );
  await expect
    .poll(
      async () =>
        (
          await queryRows<{ finished_at: string | null }>(
            "SELECT finished_at FROM mail_deliveries WHERE id = $1",
            [id],
          )
        )[0]?.finished_at,
      { timeout: 60_000, intervals: [250] },
    )
    .not.toBeNull();
}

let user: SeededUser | undefined;
let ctx: BrowserContext | undefined;

test.afterEach(async () => {
  await ctx?.close().catch(() => {});
  await user?.cleanup();
});

test("digest setting persists via UI; a real state change + SendDigest produces a mailpit email with a working unsubscribe link", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(120_000);

  // A dedicated user owns its monitor, subscription and digest watermark, so concurrent specs can't
  // leak into its digest.
  user = await seedBuiltinRoleUser(await loginAsAdmin(), "digest", "Admin");
  const api = new E2EApiClient();
  await api.login(user.email, user.password);

  ctx = await browser.newContext({ timezoneId: CONTEXT_TIMEZONE });
  const page = await ctx.newPage();
  await loginAsAnyLanding(page, user.email, user.password);

  await page.goto("/settings/notifications");
  await expect(page.getByRole("button", { name: "Back to settings" })).toBeVisible();
  const toggle = page.locator("#digest-toggle");
  await expect(toggle).toHaveAttribute("data-state", "unchecked", { timeout: 8000 });
  await expect(page.locator("#digest-hour")).toHaveCount(0);
  await expect(page.locator("#digest-timezone")).toHaveCount(0);

  await toggle.click();
  await expect(page.getByText("Saved.", { exact: true })).toBeVisible({ timeout: 8000 });
  await expect(toggle).toHaveAttribute("data-state", "checked");

  // First enable defaults to hour 8 / the pinned timezone, sent with the toggle.
  await expect(page.locator("#digest-hour")).toContainText("08:00");
  await expect(page.locator("#digest-timezone")).toContainText("UTC");
  await expect(page.getByText("You'll get your digest at 08:00 each day, UTC time.")).toBeVisible();
  await expect(page.getByText("Detected from your browser.")).toBeVisible();

  await page.reload();
  await expect(page.locator("#digest-toggle")).toHaveAttribute("data-state", "checked", {
    timeout: 8000,
  });
  await expect(page.locator("#digest-hour")).toContainText("08:00");
  await expect(page.locator("#digest-timezone")).toContainText("UTC");

  const userRows = await queryRows<{
    id: number;
    org_id: number;
    digest_enabled: boolean;
    digest_hour: number;
    digest_timezone: string;
  }>("SELECT id, org_id, digest_enabled, digest_hour, digest_timezone FROM users WHERE id = $1", [
    user.userId,
  ]);
  const { id: userId, org_id: orgId, digest_enabled, digest_hour, digest_timezone } = userRows[0];
  expect(digest_enabled).toBe(true);
  expect(digest_hour).toBe(8);
  expect(digest_timezone).toBe("UTC");

  await page.click("#digest-hour");
  await page.getByRole("option", { name: "17:00", exact: true }).click();
  // The hour and timezone Selects share mutation.isPending; opening the timezone one mid-save races
  // Radix's re-render, so wait for the hour save.
  await expect(page.locator("#digest-hour")).toContainText("17:00", { timeout: 8000 });
  await expect(page.locator("#digest-timezone")).toBeEnabled({ timeout: 8000 });
  await page.click("#digest-timezone");
  await page.getByRole("option", { name: "Europe/Berlin", exact: true }).click();

  await expect(
    page.getByText("You'll get your digest at 17:00 each day, Europe/Berlin time."),
  ).toBeVisible({ timeout: 8000 });
  // "Saved." only reappears once the request settled, so a reload can't abort the save.
  await expect(page.getByText("Saved.", { exact: true })).toBeVisible({ timeout: 8000 });
  await expect(page.getByText("Detected from your browser.")).toHaveCount(0);

  await page.reload();
  await expect(page.locator("#digest-hour")).toContainText("17:00", { timeout: 8000 });
  await expect(page.locator("#digest-timezone")).toContainText("Europe/Berlin");
  await expect(
    page.getByText("You'll get your digest at 17:00 each day, Europe/Berlin time."),
  ).toBeVisible();

  const afterHourChange = await queryRows<{ digest_hour: number; digest_timezone: string }>(
    "SELECT digest_hour, digest_timezone FROM users WHERE id = $1",
    [userId],
  );
  expect(afterHourChange[0].digest_hour).toBe(17);
  expect(afterHourChange[0].digest_timezone).toBe("Europe/Berlin");

  const monitorName = `e2e-digest-monitor-${Date.now()}`;

  const ds = await api.createDataSource({
    name: `e2e-digest-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    // typed_table row id=1 has int_col=42 (seeded in tests/e2e/seed/target-db.sql).
    name: `e2e-digest-q-${Date.now()}`,
    query: "SELECT int_col AS count FROM typed_table WHERE id = 1",
  });
  await api.publishQuery(q.id);
  const monitor = await api.createMonitor({
    query_id: q.id,
    name: monitorName,
    options: { column: "count", op: ">", value: 0 },
  });
  // Subscribe directly (no destination; the -1 sentinel notifies the subscriber's own email).
  await api.createMonitorSubscription(monitor.id, { destination_id: null });

  try {
    await executeAndWait(api, q.id);
    await enqueueEvaluateMonitor(orgId, monitor.id);

    await expect
      .poll(
        async () => {
          const r = await queryRows<{ state: string }>(
            "SELECT state FROM monitors WHERE id = $1",
            [monitor.id],
          );
          return r[0]?.state;
        },
        { timeout: 90_000, intervals: [2000] },
      )
      .toBe("triggered");

    await sendDigestAndWait(orgId, userId);

    const message = await pollDigestMailpitMessage(user.email, 30_000);
    expect(message.Subject).toMatch(/arcodash digest — 1 monitor update/);

    const full = await fetchMailpitMessage(message.ID);
    expect(full.Text).toContain(monitorName);
    expect(full.Text).toContain("Not checked yet → Alerting");
    expect(full.Text).toContain("Unsubscribe:");

    expect(full.HTML.length).toBeGreaterThan(0);
    expect(full.HTML).toContain(monitorName);
    expect(full.HTML).toMatch(/href="[^"]*\/api\/digest\/unsubscribe\?token=[^"]*"/);

    // Enqueue again with no state change: the watermark keeps the old change out of "Changes" and
    // the Alerting reminder isn't due until TRIGGERED_REMINDER_INTERVAL, so no second email.
    expect(await digestMessagesFor(user.email)).toHaveLength(1);
    await sendDigestAndWait(orgId, userId);
    expect(await digestMessagesFor(user.email)).toHaveLength(1);

    const match = full.Text.match(/Unsubscribe:\s+(\S+)/);
    expect(match).not.toBeNull();
    const unsubscribeUrl = match![1];

    const unsubRes = await fetch(unsubscribeUrl);
    expect(unsubRes.status).toBe(200);
    const unsubPage = await unsubRes.text();
    expect(unsubPage).toContain("You're unsubscribed");

    await expect
      .poll(
        async () => {
          const r = await queryRows<{ digest_enabled: boolean }>(
            "SELECT digest_enabled FROM users WHERE id = $1",
            [userId],
          );
          return r[0]?.digest_enabled;
        },
        { timeout: 10_000, intervals: [500] },
      )
      .toBe(false);

    await sendDigestAndWait(orgId, userId);
    expect(await digestMessagesFor(user.email)).toHaveLength(1);
  } finally {
    await api.deleteMonitor(monitor.id).catch(() => {});
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});

test("digest carries a Dashboards brief for a favorite dashboard; narratives off leaves the digest unsent", async ({}, testInfo) => {
  testInfo.setTimeout(120_000);
  const admin = await loginAsAdmin();
  user = await seedBuiltinRoleUser(admin, "digest-dash", "Admin");
  const api = new E2EApiClient();
  await api.login(user.email, user.password);
  const [{ org_id: orgId }] = await queryRows<{ org_id: number }>(
    "SELECT org_id FROM users WHERE id = $1",
    [user.userId],
  );
  // Enabling through the API mints the unsubscribe token the worker requires; set one directly.
  await queryRows(
    "UPDATE users SET digest_enabled = true, digest_unsubscribe_token = $2 WHERE id = $1",
    [user.userId, `e2e-unsub-${user.userId}-${Date.now()}`],
  );

  const stub = await startStub();
  const restore = await useStubProvider(admin, stub.baseUrl);
  const stamp = Date.now();
  let ds: { id: number } | undefined;
  let q: { id: number } | undefined;
  let viz: { id: number } | undefined;
  let dash: { id: number; slug: string; name: string } | undefined;
  try {
    ds = await api.createDataSource({
      name: `e2e-digest-dash-ds-${stamp}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-digest-dash-q-${stamp}`,
      query: "SELECT 4242 AS orders_total",
    });
    await api.publishQuery(q.id);
    viz = await api.createVisualization({
      query_id: q.id,
      type: "table",
      name: `e2e-digest-dash-viz-${stamp}`,
      options: {},
    });
    await executeAndWait(api, q.id);
    dash = await api.createDashboard(`e2e-digest-dash-${stamp}`);
    await api.createWidget({
      dashboard_id: dash.id,
      visualization_id: viz.id,
      text: null,
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
    });
    await api.starDashboard(dash.id);

    await sendDigestAndWait(orgId, user.userId);
    const message = await pollDigestMailpitMessage(user.email, 30_000);
    expect(message.Subject).toBe(`arcodash digest — ${dash.name}`);
    const full = await fetchMailpitMessage(message.ID);
    expect(full.Text).toContain("Dashboards");
    expect(full.Text).toContain(STUB_SUMMARY);
    expect(full.Text).toContain(dash.name);
    expect(stub.prompts.length).toBeGreaterThan(0);

    await admin.rawFetch("/api/organization/ai", {
      method: "POST",
      body: JSON.stringify({ dashboard_narratives: false }),
    });
    const calls = stub.prompts.length;
    await sendDigestAndWait(orgId, user.userId);
    expect(await digestMessagesFor(user.email)).toHaveLength(1);
    expect(stub.prompts.length).toBe(calls);
  } finally {
    if (dash) await api.deleteDashboard(dash.slug).catch(() => {});
    if (viz) await api.deleteVisualization(viz.id).catch(() => {});
    if (q) await api.deleteQuery(q.id).catch(() => {});
    if (ds) await api.deleteDataSource(ds.id).catch(() => {});
    await restore();
    await stub.close();
  }
});
