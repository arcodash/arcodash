// Direct Postgres helpers for asserting persisted state.
import { Client } from "pg";

const TEST_DATABASE_URL =
  process.env.TEST_DATABASE_URL ?? "postgres://arcodash:arcodash@localhost:5433/arcodash_test";

async function withClient<T>(fn: (client: Client) => Promise<T>): Promise<T> {
  const client = new Client({ connectionString: TEST_DATABASE_URL });
  await client.connect();
  try {
    return await fn(client);
  } finally {
    await client.end();
  }
}

export async function getUser(email: string) {
  return withClient(async (c) => {
    const res = await c.query(
      "SELECT id, email, name, groups, org_id, is_invitation_pending, disabled_at FROM users WHERE email = $1",
      [email],
    );
    return res.rows[0] ?? null;
  });
}

// `published` mirrors the API field (published_change_id IS NOT NULL).
export async function getQuery(name: string) {
  return withClient(async (c) => {
    const res = await c.query(
      "SELECT id, name, query, org_id, user_id, is_archived, working_version, (published_change_id IS NOT NULL AND unpublished_at IS NULL) AS published FROM queries WHERE name = $1 LIMIT 1",
      [name],
    );
    return res.rows[0] ?? null;
  });
}

export async function countRows(
  table: string,
  where?: string,
  params?: unknown[],
): Promise<number> {
  return withClient(async (c) => {
    const sql = `SELECT COUNT(*) FROM ${table}${where ? ` WHERE ${where}` : ""}`;
    const res = await c.query(sql, params);
    return parseInt(res.rows[0].count, 10);
  });
}

export async function deleteRows(
  table: string,
  where: string,
  params: unknown[],
): Promise<void> {
  return withClient(async (c) => {
    await c.query(`DELETE FROM ${table} WHERE ${where}`, params);
  });
}

export async function queryRows<T = Record<string, unknown>>(
  sql: string,
  params?: unknown[],
): Promise<T[]> {
  return withClient(async (c) => {
    const res = await c.query(sql, params);
    return res.rows as T[];
  });
}
