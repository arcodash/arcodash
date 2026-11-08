import { useRef } from "react";
import { useQuery } from "@tanstack/react-query";
import { ApiError, api } from "@/lib/api";
import { JobStatus, type JobStatusValue } from "@/types/api";

const TERMINAL_STATUSES: ReadonlySet<JobStatusValue> = new Set([
  JobStatus.SUCCESS,
  JobStatus.FAILURE,
  JobStatus.CANCELLED,
]);

const POLL_ERROR_MESSAGE =
  "Lost track of this run — it may have finished without us seeing it. Try running it again.";

const POLL_GAVE_UP_MESSAGE =
  "This run hasn't reported back. It may still be going — reload to check.";

// ≈5 min (3 @800ms + 3 @1.6s + 74 @4s): long enough for a slow warehouse, bounded so a wedged
// worker does not poll forever.
const POLL_MAX_ATTEMPTS = 80;

export function usePollJob(jobId: string | null, intervalMs = 800) {
  // react-query exposes no attempt count outside the refetchInterval callback, so track it by hand;
  // reset during render so a new jobId cannot inherit a stale count.
  const jobIdRef = useRef<string | null>(null);
  const attemptRef = useRef(0);
  if (jobIdRef.current !== jobId) {
    jobIdRef.current = jobId;
    attemptRef.current = 0;
  }

  const query = useQuery({
    queryKey: ["job", jobId],
    queryFn: () => {
      attemptRef.current += 1;
      return api.getJob(jobId!);
    },
    enabled: jobId != null,
    // 404 means the job's TTL expired, so retrying will not help; anything else gets a couple of
    // retries.
    retry: (failureCount, error) => {
      if (error instanceof ApiError && error.status === 404) return false;
      return failureCount < 2;
    },
    refetchInterval: (q) => {
      // Once retries are exhausted the query never recovers; stop polling a gone job or a down
      // backend.
      if (q.state.status === "error") return false;
      const status = q.state.data?.job.status;
      if (status != null && TERMINAL_STATUSES.has(status)) return false;
      // Step the interval up as a job runs, then stop, so a wedged job does not poll forever.
      const attempt = attemptRef.current;
      if (attempt >= POLL_MAX_ATTEMPTS) return false;
      if (attempt < 3) return intervalMs;
      if (attempt < 6) return intervalMs * 2;
      return intervalMs * 5;
    },
    // Pauses in a hidden tab so it does not burn the attempt budget.
    refetchIntervalInBackground: false,
  });

  const job = query.data?.job ?? null;
  const status = job?.status ?? null;
  // The caller lost track of the job (as opposed to the job failing).
  const isPollError = query.isError;
  const isTerminal = status != null && TERMINAL_STATUSES.has(status);
  // Stopped asking before a terminal status; the job may still run. Guarded on `!isFetching` so it
  // cannot flip while the final attempt may still resolve.
  const gaveUp = !isTerminal && !isPollError && !query.isFetching && attemptRef.current >= POLL_MAX_ATTEMPTS;
  const isDone = isTerminal || isPollError || gaveUp;

  return {
    job,
    status,
    queryResultId: job?.query_result_id ?? null,
    error: isPollError ? POLL_ERROR_MESSAGE : gaveUp ? POLL_GAVE_UP_MESSAGE : (job?.error ?? null),
    isDone,
    isPending: status === JobStatus.PENDING && !isPollError && !gaveUp,
    isStarted: status === JobStatus.STARTED && !isPollError && !gaveUp,
    isSuccess: status === JobStatus.SUCCESS,
    isFailure: status === JobStatus.FAILURE || isPollError,
    isCancelled: status === JobStatus.CANCELLED,
    /** The poll itself broke (404/exhausted retries), not the job; nothing for "Fix with AI" to
     * fix. */
    isPollError,
    /** Attempt ceiling reached with the job still non-terminal; it may still be running. */
    gaveUp,
    isLoading: query.isLoading,
    queryState: query,
  };
}
