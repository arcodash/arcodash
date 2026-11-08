/** E2E: the scheduler fires a real scheduled query against the real target Postgres. Most tests
 * seed `schedules` via SQL and nudge the leader loop with publishWake() instead of waiting out its
 * 120s sleep. */
import { test, expect } from "@playwright/test";
import { E2EApiClient, loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { deleteRows, queryRows } from "./helpers/db";
import { publishWake } from "./helpers/valkey";
import { MAILPIT_URL } from "./helpers/mailpit";

/** Seed a query_results row ageSeconds old holding {n: rowValue} as the draft result (publishQuery
 * then makes it the published pointer too). */
async function seedStaleResult(
  orgId: number,
  dsId: number,
  queryId: number,
  rowValue: number,
  ageSeconds: number,
): Promise<number> {
  const rows = await queryRows<{ id: number }>(
    `INSERT INTO query_results (org_id, data_source_id, query_hash, query, data, runtime, retrieved_at, created_at, updated_at)
     VALUES ($1, $2, $3, 'seed', $4, 0.01, now() - ($5 || ' seconds')::interval, now(), now())
     RETURNING id`,
    [
      orgId,
      dsId,
      `seed-hash-${Date.now()}-${Math.random()}`,
      JSON.stringify({ rows: [{ n: rowValue }] }),
      ageSeconds,
    ],
  );
  const qrId = rows[0].id;
  await queryRows("UPDATE queries SET latest_query_data_id = $1 WHERE id = $2", [qrId, queryId]);
  return qrId;
}

/** Seed a schedules row, overwriting the default one POST /api/monitors auto-creates (upsert; a
 * plain INSERT would collide with uq_schedules_entity_type_entity_id). */
async function seedMonitorSchedule(
  orgId: number,
  monitorId: number,
  everySecs = 60,
): Promise<void> {
  await queryRows(
    `INSERT INTO schedules (org_id, entity_type, entity_id, spec, timezone, enabled, overlap_policy, next_run_at)
     VALUES ($1, 'monitor', $2, $3, 'UTC', true, 'skip', now())
     ON CONFLICT (entity_type, entity_id) DO UPDATE SET
       spec = EXCLUDED.spec, enabled = true, overlap_policy = 'skip', next_run_at = now()`,
    [orgId, monitorId, JSON.stringify({ kind: "interval", every_secs: everySecs })],
  );
}

interface MonitorRunRow {
  id: number;
  status: string;
  waiting_on_run_id: number | null;
  query_result_id: number | null;
  data_age_secs: number | null;
  error: string | null;
}

/** Poll until the monitor's latest run is terminal, re-publishing the wake each time: the periodic
 * sweep only runs when the loop wakes. */
async function pollLatestMonitorRunToTerminal(
  monitorId: number,
  timeoutMs: number,
): Promise<MonitorRunRow> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const rows = await queryRows<MonitorRunRow>(
      "SELECT id, status, waiting_on_run_id, query_result_id, data_age_secs, error FROM runs \
       WHERE entity_type = 'monitor' AND entity_id = $1 ORDER BY id DESC LIMIT 1",
      [monitorId],
    );
    const row = rows[0];
    if (row && (row.status === "success" || row.status === "failed")) return row;
    await publishWake();
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`monitor ${monitorId}'s run did not finalize within ${timeoutMs}ms`);
}

async function pollLatestMonitorRunParked(monitorId: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const rows = await queryRows<MonitorRunRow>(
      "SELECT id, status, waiting_on_run_id, query_result_id, data_age_secs, error FROM runs \
       WHERE entity_type = 'monitor' AND entity_id = $1 ORDER BY id DESC LIMIT 1",
      [monitorId],
    );
    if (rows[0]?.waiting_on_run_id != null) return;
    await publishWake();
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`monitor ${monitorId}'s run never parked within ${timeoutMs}ms`);
}

async function mailpitHasMessageContaining(substring: string): Promise<boolean> {
  const res = await fetch(`${MAILPIT_URL}/api/v1/messages?limit=50`);
  if (!res.ok) return false;
  const body = (await res.json()) as { messages: { Subject: string }[] };
  return body.messages?.some((m) => m.Subject?.includes(substring)) ?? false;
}

test("a schedules row fires a real query run: published_latest_query_data_id advances and a runs row lands with real status/duration/row_count", async ({}, testInfo) => {
  testInfo.setTimeout(75_000);

  const api = await loginAsAdmin();
  const suffix = Date.now();

  const ds = await api.createDataSource({
    name: `e2e-sched-v2-ds-${suffix}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  // Never executed manually, so published_latest_query_data_id starts NULL; schedules only run the
  // PUBLISHED version, so publish before seeding.
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-sched-v2-q-${suffix}`,
    query: "SELECT int_col AS count FROM typed_table WHERE id = 1",
  });

  try {
    await api.publishQuery(q.id);

    const before = await queryRows<{ published_latest_query_data_id: number | null }>(
      "SELECT published_latest_query_data_id FROM queries WHERE id = $1",
      [q.id],
    );
    expect(before[0].published_latest_query_data_id).toBeNull();

    const orgRows = await queryRows<{ org_id: number }>(
      "SELECT org_id FROM queries WHERE id = $1",
      [q.id],
    );
    const orgId = orgRows[0].org_id;

    await queryRows(
      `INSERT INTO schedules (org_id, entity_type, entity_id, spec, timezone, enabled, overlap_policy, next_run_at)
       VALUES ($1, 'query', $2, $3, 'UTC', true, 'skip', now())`,
      [orgId, q.id, JSON.stringify({ kind: "interval", every_secs: 60 })],
    );

    // Re-publish the wake each poll: a wake sent before the subscriber connects is dropped, not
    // queued. Latency varies, so allow a generous margin.
    await expect
      .poll(
        async () => {
          await publishWake();
          const rows = await queryRows<{ published_latest_query_data_id: number | null }>(
            "SELECT published_latest_query_data_id FROM queries WHERE id = $1",
            [q.id],
          );
          return rows[0]?.published_latest_query_data_id ?? null;
        },
        { timeout: 45_000, intervals: [1000] },
      )
      .not.toBeNull();

    const afterQuery = await queryRows<{ published_latest_query_data_id: number }>(
      "SELECT published_latest_query_data_id FROM queries WHERE id = $1",
      [q.id],
    );
    const queryResultId = afterQuery[0].published_latest_query_data_id;

    const runs = await queryRows<{
      status: string;
      duration_ms: number | null;
      row_count: number | null;
      query_result_id: number | null;
      triggered_by: string;
    }>(
      "SELECT status, duration_ms, row_count, query_result_id, triggered_by FROM runs \
       WHERE entity_type = 'query' AND entity_id = $1 ORDER BY id DESC LIMIT 1",
      [q.id],
    );

    expect(runs).toHaveLength(1);
    expect(runs[0].triggered_by).toBe("schedule");
    expect(runs[0].status).toBe("success");
    expect(runs[0].duration_ms).not.toBeNull();
    expect(runs[0].row_count).not.toBeNull();
    expect(runs[0].query_result_id).toBe(queryResultId);
  } finally {
    await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(() => {});
    await deleteRows("schedules", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(
      () => {},
    );
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});

interface ScheduleResponseBody {
  id: number;
  next_run_at: string | null;
}

interface RunResponseBody {
  id: number;
  status: string;
  duration_ms: number | null;
  row_count: number | null;
  query_result_id: number | null;
}

/** Poll GET /api/runs (the control-plane read model) until the newest run is terminal. */
async function pollRunsApiToTerminal(
  api: E2EApiClient,
  entityType: "query" | "monitor",
  entityId: number,
  timeoutMs: number,
): Promise<RunResponseBody> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const runs = await api.rawFetch<RunResponseBody[]>(
      `/api/runs?entity_type=${entityType}&entity_id=${entityId}`,
    );
    const newest = runs[0];
    if (newest && (newest.status === "success" || newest.status === "failed")) return newest;
    await publishWake();
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`${entityType} ${entityId}'s run did not finalize within ${timeoutMs}ms`);
}

test("the control-plane API (not direct SQL) creates a schedule, fires a real run, and its CSV/run-now paths work end to end", async ({}, testInfo) => {
  // A new interval schedule's first fire is every_secs plus jitter away (no fire-on-create), so a
  // 60s schedule can take ~70s.
  testInfo.setTimeout(120_000);

  const api = await loginAsAdmin();
  const suffix = Date.now();

  const ds = await api.createDataSource({
    name: `e2e-sched-api-ds-${suffix}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-sched-api-q-${suffix}`,
    query: "SELECT int_col AS count FROM typed_table WHERE id = 1",
  });

  try {
    await api.publishQuery(q.id);

    const schedule = await api.rawFetch<ScheduleResponseBody>("/api/schedules", {
      method: "POST",
      body: JSON.stringify({
        entity_type: "query",
        entity_id: q.id,
        spec: { kind: "interval", every_secs: 60 },
      }),
    });
    expect(schedule.id).toBeTruthy();
    expect(schedule.next_run_at).not.toBeNull();

    await publishWake();

    const scheduledRun = await pollRunsApiToTerminal(api, "query", q.id, 90_000);
    expect(scheduledRun.status).toBe("success");
    expect(scheduledRun.duration_ms).not.toBeNull();
    expect(scheduledRun.row_count).not.toBeNull();
    expect(scheduledRun.query_result_id).not.toBeNull();

    const backendUrl = process.env.BACKEND_URL ?? "http://localhost:8080";
    const csvRes = await fetch(`${backendUrl}/api/runs/${scheduledRun.id}/result.csv`, {
      headers: { Cookie: api.sessionCookieHeader },
    });
    expect(csvRes.status).toBe(200);
    const csvBody = await csvRes.text();
    expect(csvBody.toLowerCase()).toContain("count");

    const manualRun = await api.rawFetch<RunResponseBody>(`/api/queries/${q.id}/run`, {
      method: "POST",
    });
    expect(manualRun.id).toBeTruthy();
    await pollRunsApiToTerminal(api, "query", q.id, 30_000);

    // A second run-now with nothing in flight is a fresh run, not a dupe.
    const secondManualRun = await api.rawFetch<RunResponseBody>(`/api/queries/${q.id}/run`, {
      method: "POST",
    });
    expect(secondManualRun.id).toBeTruthy();
  } finally {
    await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(() => {});
    await deleteRows("schedules", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(
      () => {},
    );
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});

/** Monitor freshness protocol against a genuinely slow (20s pg_sleep) query, so parking, waiting
 * and resume use real timing. */
test.describe("monitor freshness: attach-or-trigger + resume", () => {
  test("default always-wait: parks behind the real 20s query, then evaluates the fresh result", async ({}, testInfo) => {
    testInfo.setTimeout(90_000);
    const api = await loginAsAdmin();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-fresh-a-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-fresh-a-q-${suffix}`,
      query: "SELECT count(*)::int AS n FROM (SELECT pg_sleep(20)) s",
    });

    let monitorId: number | undefined;
    try {
      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM queries WHERE id = $1",
        [q.id],
      );
      const orgId = orgRows[0].org_id;

      // n=0 and too old to be fresh: the monitor must trigger a refresh.
      await seedStaleResult(orgId, ds.id, q.id, 0, 10_000);
      await api.publishQuery(q.id);

      const monitor = await api.createMonitor({
        query_id: q.id,
        name: `e2e-fresh-a-mon-${suffix}`,
        options: { column: "n", op: ">", value: 0 },
      });
      monitorId = monitor.id;
      await seedMonitorSchedule(orgId, monitor.id);

      await publishWake();

      await pollLatestMonitorRunParked(monitor.id, 45_000);

      const finalRun = await pollLatestMonitorRunToTerminal(monitor.id, 30_000);
      expect(finalRun.status).toBe("success");
      expect(finalRun.waiting_on_run_id).toBeNull();
      expect(finalRun.data_age_secs).not.toBeNull();
      expect(Number(finalRun.data_age_secs)).toBeLessThan(10);

      // Evaluated the NEW result (n=1), not the stale seed.
      const qr = await queryRows<{ data: { rows: { n: number }[] } }>(
        "SELECT data FROM query_results WHERE id = $1",
        [finalRun.query_result_id],
      );
      expect(qr[0].data.rows[0].n).toBe(1);

      const monitorRow = await queryRows<{ state: string }>(
        "SELECT state FROM monitors WHERE id = $1",
        [monitor.id],
      );
      expect(monitorRow[0].state).toBe("triggered");
    } finally {
      if (monitorId) {
        await deleteRows("runs", "entity_type = 'monitor' AND entity_id = $1", [monitorId]).catch(
          () => {},
        );
        await deleteRows("schedules", "entity_type = 'monitor' AND entity_id = $1", [
          monitorId,
        ]).catch(() => {});
        await api.deleteMonitor(monitorId).catch(() => {});
      }
      await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
      await api.deleteDataSource(ds.id).catch(() => {});
    }
  });

  test("max_fresh_wait_secs=5: detaches at the ceiling and evaluates the labeled-stale result; the slow query is left to finish on its own", async ({}, testInfo) => {
    // The sweep that owns the ceiling detach runs only on loop wake (~60s cadence, longer under
    // load); a 150s query leaves room for the detach to land first.
    testInfo.setTimeout(300_000);
    const api = await loginAsAdmin();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-fresh-b-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-fresh-b-q-${suffix}`,
      query: "SELECT count(*)::int AS n FROM (SELECT pg_sleep(150)) s",
    });

    let monitorId: number | undefined;
    try {
      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM queries WHERE id = $1",
        [q.id],
      );
      const orgId = orgRows[0].org_id;
      const seededQrId = await seedStaleResult(orgId, ds.id, q.id, 0, 10_000);
      await api.publishQuery(q.id);

      const monitor = await api.createMonitor({
        query_id: q.id,
        name: `e2e-fresh-b-mon-${suffix}`,
        options: { column: "n", op: ">", value: 0 },
      });
      monitorId = monitor.id;
      await queryRows("UPDATE monitors SET options = options || $1::jsonb WHERE id = $2", [
        JSON.stringify({ max_fresh_wait_secs: 5 }),
        monitor.id,
      ]);
      // Long interval so a second tick of the monitor's own schedule doesn't confound the
      // no-duplicate-resume assertion.
      await seedMonitorSchedule(orgId, monitor.id, 600);

      await publishWake();

      const start = Date.now();
      const finalRun = await pollLatestMonitorRunToTerminal(monitor.id, 145_000);
      const elapsedMs = Date.now() - start;
      expect(elapsedMs).toBeLessThan(145_000);
      expect(finalRun.status).toBe("success");
      expect(finalRun.query_result_id).toBe(seededQrId);
      expect(Number(finalRun.data_age_secs)).toBeGreaterThan(5);

      const inFlight = await queryRows<{ status: string }>(
        "SELECT status FROM runs WHERE entity_type = 'query' AND entity_id = $1 \
         ORDER BY id DESC LIMIT 1",
        [q.id],
      );
      expect(["pending", "running"]).toContain(inFlight[0].status);

      // The finished slow query must not re-fire the detached monitor.
      const untilQueryDoneMs = Math.max(210_000 - (Date.now() - start), 30_000);
      await expect
        .poll(
          async () => {
            await publishWake();
            const rows = await queryRows<{ status: string }>(
              "SELECT status FROM runs WHERE entity_type = 'query' AND entity_id = $1 \
               ORDER BY id DESC LIMIT 1",
              [q.id],
            );
            return rows[0]?.status;
          },
          { timeout: untilQueryDoneMs, intervals: [1000] },
        )
        .toBe("success");

      const monitorRunCount = await queryRows<{ count: string }>(
        "SELECT count(*) FROM runs WHERE entity_type = 'monitor' AND entity_id = $1",
        [monitor.id],
      );
      expect(Number(monitorRunCount[0].count)).toBe(1);
    } finally {
      if (monitorId) {
        await deleteRows("runs", "entity_type = 'monitor' AND entity_id = $1", [monitorId]).catch(
          () => {},
        );
        await deleteRows("schedules", "entity_type = 'monitor' AND entity_id = $1", [
          monitorId,
        ]).catch(() => {});
        await api.deleteMonitor(monitorId).catch(() => {});
      }
      await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
      await api.deleteDataSource(ds.id).catch(() => {});
    }
  });

  test("a failing refresh fails the monitor run and never notifies off stale data", async ({}, testInfo) => {
    testInfo.setTimeout(90_000);
    const api = await loginAsAdmin();
    const suffix = Date.now();
    const monitorName = `e2e-fresh-c-mon-${suffix}`;

    const ds = await api.createDataSource({
      name: `e2e-fresh-c-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-fresh-c-q-${suffix}`,
      query: "SELECT this is not valid sql",
    });
    const destination = await api.createDestination({
      name: `e2e-fresh-c-dest-${suffix}`,
      type: "email",
      options: { to: `${monitorName}@example.com` },
    });

    let monitorId: number | undefined;
    try {
      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM queries WHERE id = $1",
        [q.id],
      );
      const orgId = orgRows[0].org_id;
      // n=100 > 0 would trigger if the stale value were ever re-evaluated.
      await seedStaleResult(orgId, ds.id, q.id, 100, 10_000);
      await api.publishQuery(q.id);

      const monitor = await api.createMonitor({
        query_id: q.id,
        name: monitorName,
        options: { column: "n", op: ">", value: 0 },
      });
      monitorId = monitor.id;
      await api.createMonitorSubscription(monitor.id, { destination_id: destination.id });
      await seedMonitorSchedule(orgId, monitor.id);

      await publishWake();

      const finalRun = await pollLatestMonitorRunToTerminal(monitor.id, 45_000);
      expect(finalRun.status).toBe("failed");
      expect(finalRun.error).toMatch(/^query run #\d+ failed: /);
      expect(finalRun.error).toContain("syntax error");

      const schedRows = await queryRows<{ consecutive_failures: number }>(
        "SELECT consecutive_failures FROM schedules WHERE entity_type = 'monitor' AND entity_id = $1",
        [monitor.id],
      );
      expect(schedRows[0].consecutive_failures).toBeGreaterThanOrEqual(1);

      // Give any incorrect notification time to land.
      await new Promise((r) => setTimeout(r, 3000));
      expect(await mailpitHasMessageContaining(monitorName)).toBe(false);
    } finally {
      if (monitorId) {
        await deleteRows("runs", "entity_type = 'monitor' AND entity_id = $1", [monitorId]).catch(
          () => {},
        );
        await deleteRows("schedules", "entity_type = 'monitor' AND entity_id = $1", [
          monitorId,
        ]).catch(() => {});
        await api.deleteMonitor(monitorId).catch(() => {});
      }
      await api.deleteDestination(destination.id).catch(() => {});
      await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
      await api.deleteDataSource(ds.id).catch(() => {});
    }
  });

  test("a herd of monitors on one stale slow query collapses to a single query run", async ({}, testInfo) => {
    testInfo.setTimeout(120_000);
    const api = await loginAsAdmin();
    const suffix = Date.now();
    const HERD_SIZE = 5;

    const ds = await api.createDataSource({
      name: `e2e-fresh-herd-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-fresh-herd-q-${suffix}`,
      query: "SELECT count(*)::int AS n FROM (SELECT pg_sleep(20)) s",
    });

    const monitorIds: number[] = [];
    try {
      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM queries WHERE id = $1",
        [q.id],
      );
      const orgId = orgRows[0].org_id;
      await seedStaleResult(orgId, ds.id, q.id, 0, 10_000);
      await api.publishQuery(q.id);

      for (let i = 0; i < HERD_SIZE; i++) {
        const monitor = await api.createMonitor({
          query_id: q.id,
          name: `e2e-fresh-herd-mon-${suffix}-${i}`,
          options: { column: "n", op: ">", value: 0 },
        });
        monitorIds.push(monitor.id);
        await seedMonitorSchedule(orgId, monitor.id);
      }

      await publishWake();

      for (const id of monitorIds) {
        const finalRun = await pollLatestMonitorRunToTerminal(id, 60_000);
        expect(finalRun.status).toBe("success");
      }

      const inflightQueryRuns = await queryRows<{ count: string }>(
        "SELECT count(*) FROM runs WHERE entity_type = 'query' AND entity_id = $1 AND triggered_by = 'monitor'",
        [q.id],
      );
      expect(Number(inflightQueryRuns[0].count)).toBe(1);
    } finally {
      for (const id of monitorIds) {
        await deleteRows("runs", "entity_type = 'monitor' AND entity_id = $1", [id]).catch(
          () => {},
        );
        await deleteRows("schedules", "entity_type = 'monitor' AND entity_id = $1", [id]).catch(
          () => {},
        );
        await api.deleteMonitor(id).catch(() => {});
      }
      await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
      await api.deleteDataSource(ds.id).catch(() => {});
    }
  });
});
