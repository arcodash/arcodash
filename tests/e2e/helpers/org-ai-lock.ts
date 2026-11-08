/** Cross-worker lock for specs that change org-wide AI state (provider, budget cap, sample-value
 * flags). Playwright workers share one org, so concurrent specs would see each other's settings. */
import { mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BASE_URL = process.env.PLAYWRIGHT_BASE_URL ?? "http://localhost:3000";
// Keyed on the stack under test so separate stacks do not block each other.
const LOCK_DIR = join(tmpdir(), `arcodash-e2e-org-ai-${BASE_URL.replace(/[^a-zA-Z0-9]+/g, "_")}.lock`);
const PID_FILE = join(LOCK_DIR, "pid");
// Backstop for a holder whose pid is unreadable or recycled.
const STALE_MS = 5 * 60_000;

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return (e as NodeJS.ErrnoException).code === "EPERM";
  }
}

// A worker runs one test at a time, so a lock held by this pid is leaked.
function isStale(): boolean {
  try {
    const pid = Number(readFileSync(PID_FILE, "utf8"));
    if (Number.isInteger(pid) && pid > 0) return pid === process.pid || !isAlive(pid);
  } catch {
    // Holder has not written its pid yet (or released): fall back to age.
  }
  return Date.now() - statSync(LOCK_DIR).mtimeMs > STALE_MS;
}

export async function acquireOrgAiLock(): Promise<() => void> {
  for (;;) {
    try {
      mkdirSync(LOCK_DIR);
      writeFileSync(PID_FILE, String(process.pid));
      return () => rmSync(LOCK_DIR, { recursive: true, force: true });
    } catch {
      try {
        // Re-check right before removing so two waiters do not both clear a lock a third just took.
        if (isStale() && isStale()) rmSync(LOCK_DIR, { recursive: true, force: true });
      } catch {
        // Released between the failed mkdir and the stat: retry immediately.
        continue;
      }
      await new Promise((r) => setTimeout(r, 500));
    }
  }
}
