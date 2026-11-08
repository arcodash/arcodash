/** Minimal RESP client (Node net) to push jobs straight onto the worker's Valkey queue instead of
 * waiting on the scheduler tick. */
import { connect } from "node:net";

const VALKEY_HOST = "localhost";
const VALKEY_PORT = 6380; // docker-compose .testing.yml mapping for the test Valkey

function encodeCommand(args: string[]): string {
  let out = `*${args.length}\r\n`;
  for (const a of args) out += `$${Buffer.byteLength(a)}\r\n${a}\r\n`;
  return out;
}

function sendCommand(args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    const socket = connect({ host: VALKEY_HOST, port: VALKEY_PORT });
    let buf = "";
    socket.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      socket.end();
    });
    socket.on("close", () => resolve(buf));
    socket.on("error", reject);
    socket.on("connect", () => socket.write(encodeCommand(args)));
  });
}

export async function lpush(key: string, value: string): Promise<void> {
  const reply = await sendCommand(["LPUSH", key, value]);
  if (reply.startsWith("-")) throw new Error(`Valkey LPUSH failed: ${reply}`);
}

/** Enqueue JobMessage::EvaluateMonitor{org_id, monitor_id}. Ids are coerced with Number() so the
 * JSON is unquoted: the worker expects i64 and silently drops quoted ids. */
export async function enqueueEvaluateMonitor(
  orgId: number | string,
  monitorId: number | string,
): Promise<void> {
  const payload = JSON.stringify({
    type: "EvaluateMonitor",
    org_id: Number(orgId),
    monitor_id: Number(monitorId),
  });
  await lpush("arcodash:queue:default", payload);
}

/** Enqueue JobMessage::SendDigest{org_id, user_id} (skips the 08:00 UTC cron); same id coercion as
 * above. */
export async function enqueueSendDigest(
  orgId: number | string,
  userId: number | string,
): Promise<void> {
  const payload = JSON.stringify({
    type: "SendDigest",
    org_id: Number(orgId),
    user_id: Number(userId),
  });
  await lpush("arcodash:queue:default", payload);
}

/** PUBLISH arcodash:sched:wake 1 — nudges the scheduler leader loop to re-read MIN(next_run_at)
 * instead of waiting out its 120s sleep. */
export async function publishWake(): Promise<void> {
  await sendCommand(["PUBLISH", "arcodash:sched:wake", "1"]);
}

/** Force job:<jobId>'s TTL down to `seconds`, simulating a status record close to expiry. */
export async function expireJobStatusKey(jobId: string, seconds: number): Promise<void> {
  const reply = await sendCommand(["EXPIRE", `job:${jobId}`, String(seconds)]);
  if (reply.startsWith("-")) throw new Error(`Valkey EXPIRE failed: ${reply}`);
}

/** Current TTL (seconds) of `job:<jobId>`, or -2 if the key doesn't exist / -1 if it has no TTL. */
export async function jobStatusTtl(jobId: string): Promise<number> {
  const reply = await sendCommand(["TTL", `job:${jobId}`]);
  const match = reply.match(/:(-?\d+)\r\n/);
  if (!match) throw new Error(`Valkey TTL reply not parseable: ${reply}`);
  return Number(match[1]);
}

/** DEL a single key (e.g. a per-data-source guard that a recycled data-source id may inherit). */
export async function delKey(key: string): Promise<void> {
  const reply = await sendCommand(["DEL", key]);
  if (reply.startsWith("-")) throw new Error(`Valkey DEL failed: ${reply}`);
}
