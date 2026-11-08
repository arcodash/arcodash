/** E2E: monitor evaluation -> email via mailpit. Requires: backend :8080, worker (redis :6380),
 * target postgres :5434, mailpit :8026/:1026. Pushes EvaluateMonitor straight onto the worker
 * queue (helpers/valkey.ts) instead of waiting for the 5-minute tick; evaluate_monitor.rs reads
 * the last *cached* result, so execute the query via the API before each evaluation. */
import { test, expect } from "@playwright/test";
import { loginAsAdmin, TARGET_DS_OPTIONS, executeAndWait } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { enqueueEvaluateMonitor } from "./helpers/valkey";
import { MAILPIT_URL, pollMailpitForSubject } from "./helpers/mailpit";

test("monitor crosses threshold → triggered state + email in mailpit; recovers to ok when back below threshold", async ({}, testInfo) => {
  testInfo.setTimeout(120_000);

  const api = await loginAsAdmin();
  const monitorName = `e2e-monitor-trigger-${Date.now()}`;
  const toAddress = `${monitorName}@example.com`;

  const ds = await api.createDataSource({
    name: `e2e-monitor-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    // typed_table id=1 has int_col=42 (tests/e2e/seed/target-db.sql).
    name: `e2e-monitor-q-${Date.now()}`,
    query: "SELECT int_col AS count FROM typed_table WHERE id = 1",
  });
  await api.publishQuery(q.id); // monitor create requires a published query (require_published)
  const destination = await api.createDestination({
    name: `e2e-monitor-dest-${Date.now()}`,
    type: "email",
    options: { to: toAddress },
  });
  const monitor = await api.createMonitor({
    query_id: q.id,
    name: monitorName,
    options: { column: "count", op: ">", value: 0 },
  });
  await api.createMonitorSubscription(monitor.id, { destination_id: destination.id });

  try {
    await executeAndWait(api, q.id);

    const rows = await queryRows<{ org_id: number }>("SELECT org_id FROM monitors WHERE id = $1", [
      monitor.id,
    ]);
    const orgId = rows[0].org_id;

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

    const message = await pollMailpitForSubject(monitorName, 90_000);
    expect(message.Subject).toContain(monitorName);
    // Display label is "Alerting"; the stored state is the literal "triggered".
    expect(message.Subject.toLowerCase()).toContain("alerting");

    const full = (await (await fetch(`${MAILPIT_URL}/api/v1/message/${message.ID}`)).json()) as {
      Text?: string;
      HTML?: string;
    };
    const body = `${full.Text ?? ""}\n${full.HTML ?? ""}`;
    expect(body).toContain("What changed");
    expect(body).toContain("Crossed");

    // Recovery: execute_saved runs only the published content, so publish the edit before
    // re-executing.
    await api.rawFetch(`/api/queries/${q.id}`, {
      method: "POST",
      body: JSON.stringify({ query: "SELECT 0 AS count" }),
    });
    await api.publishQuery(q.id);
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
      .toBe("ok");
  } finally {
    await api.deleteMonitor(monitor.id).catch(() => {});
    await api.deleteDestination(destination.id).catch(() => {});
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});
