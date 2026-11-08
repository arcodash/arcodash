import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ConflictError } from "@/lib/api";

export type AutosaveStatus = "idle" | "saving" | "saved" | "failed" | "offline" | "conflict";

/** Terminal result of a save attempt, returned to the caller since same-tick state may not have
 * committed. */
export type AutosaveOutcome = "saved" | "failed" | "offline" | "conflict";

export interface UseAutosaveOptions<T> {
  enabled: boolean;
  payload: T;
  /** Reads its own concurrency token at call time. Reject with `ConflictError` for a stale-version 409;
   * anything else is a generic failure. */
  save: (payload: T) => Promise<void>;
  /** A 409 is handed to the caller; the hook only stops saving until `clearConflict`. */
  onConflict: (err: ConflictError) => void;
  debounceMs?: number;
  /** Whether the payload present when autosave turns on counts as already saved (default `true`). Pass
   * `false` when enabling is itself the first edit worth saving. */
  baselineOnEnable?: boolean;
}

export interface UseAutosaveResult {
  status: AutosaveStatus;
  /** True for any edit the server has not acknowledged (in flight, offline, failed, conflict);
   * worth blocking navigation over. */
  hasUnpersistedEdits: boolean;
  retry: () => void;
  /** Resumes saving after a conflict (retrying a stale version would conflict again); also drops the
   * pending buffer and in-flight save, since the caller is replacing the buffer with the server copy. */
  clearConflict: () => void;
  /** Persists the current payload, waiting for any in-flight save and cancelling the pending debounce.
   * No-op during a conflict. Never rejects; settles with the outcome. */
  forceSave: () => Promise<AutosaveOutcome>;
  /** Adopts the next render's payload as the saved baseline without saving (after a reload/discard
   * rehydrate, whose several setStates make the buffer unavailable synchronously). Also drops
   * pending/in-flight saves and returns to idle. */
  markClean: () => void;
  /** True if an edit has not reached the server (including the debounce window). Reads live refs, so safe
   * in a `beforeunload` handler registered once. */
  hasUnflushedChanges: () => boolean;
}

function clearTimer(timerRef: { current: ReturnType<typeof setTimeout> | null }) {
  if (timerRef.current) clearTimeout(timerRef.current);
  timerRef.current = null;
}

/** ~1s-debounced autosave with a small status machine; the only persistence path (no Save button). */
export function useAutosave<T>({
  enabled,
  payload,
  save,
  onConflict,
  debounceMs = 1000,
  baselineOnEnable = true,
}: UseAutosaveOptions<T>): UseAutosaveResult {
  const [status, setStatus] = useState<AutosaveStatus>("idle");
  // Bumped by `markClean` to force a render when payload and status are unchanged.
  const [cleanToken, setCleanToken] = useState(0);
  // Lazy-init so freshly loaded server content is not mistaken for a pending edit.
  const savedSerializedRef = useRef<string | null>(
    enabled && baselineOnEnable ? JSON.stringify(payload) : null,
  );
  const pendingRef = useRef<T | null>(null);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const inFlightRef = useRef<Promise<AutosaveOutcome> | null>(null);
  const saveRef = useRef(save);
  saveRef.current = save;
  const onConflictRef = useRef(onConflict);
  onConflictRef.current = onConflict;
  const payloadRef = useRef(payload);
  payloadRef.current = payload;
  const conflictRef = useRef(false);
  // Bumped on adopt/conflict-resolve; an in-flight save that lost compares its captured value and
  // writes nothing back.
  const generationRef = useRef(0);
  // Lets a finished save start the follow-up save without `attempt` (which must stay stable)
  // depending on itself.
  const attemptRef = useRef<(toSave: T) => Promise<AutosaveOutcome>>(null!);

  /** Drops every trace of an unsaved edit and invalidates the result of any in-flight save.
   * `inFlightRef` is left alone (its `promise.finally` clears it) so a new attempt cannot carry a
   * `working_version` the first save has not bumped. */
  const resetPending = useCallback(() => {
    generationRef.current += 1;
    clearTimer(timerRef);
    pendingRef.current = null;
    conflictRef.current = false;
    setStatus("idle");
  }, []);

  const markClean = useCallback(() => {
    setCleanToken((t) => t + 1);
    resetPending();
  }, [resetPending]);

  const attempt = useCallback((toSave: T): Promise<AutosaveOutcome> => {
    if (typeof navigator !== "undefined" && navigator.onLine === false) {
      setStatus("offline");
      return Promise.resolve<AutosaveOutcome>("offline");
    }
    const generation = generationRef.current;
    setStatus("saving");
    const promise = saveRef.current(toSave).then(
      (): AutosaveOutcome | Promise<AutosaveOutcome> => {
        // Superseded by a reset mid-flight: the request still landed, so report it saved.
        if (generationRef.current !== generation) return "saved";
        conflictRef.current = false;
        savedSerializedRef.current = JSON.stringify(toSave);
        // Only the buffer this save carried is persisted; anything newer in `pendingRef` still has
        // to go out.
        if (pendingRef.current === toSave) pendingRef.current = null;
        const next = pendingRef.current;
        if (next == null) {
          setStatus("saved");
          return "saved";
        }
        clearTimer(timerRef);
        // Returned so awaiting callers see the whole chain's final outcome.
        return attemptRef.current(next);
      },
      (err): AutosaveOutcome => {
        if (generationRef.current !== generation) return "failed";
        if (err instanceof ConflictError) {
          conflictRef.current = true;
          onConflictRef.current(err);
          setStatus("conflict");
          return "conflict";
        }
        setStatus("failed");
        return "failed";
      },
    );
    inFlightRef.current = promise;
    promise.finally(() => {
      if (inFlightRef.current === promise) inFlightRef.current = null;
    });
    return promise;
  }, []);
  attemptRef.current = attempt;

  const clearConflict = resetPending;

  // Establish the baseline when autosave turns on so the enabling payload is not a pending edit
  // (`baselineOnEnable: false` skips this).
  const wasEnabledRef = useRef(enabled);
  if (enabled && !wasEnabledRef.current && savedSerializedRef.current == null && baselineOnEnable) {
    savedSerializedRef.current = JSON.stringify(payload);
  }
  wasEnabledRef.current = enabled;

  const serialized = useMemo(() => JSON.stringify(payload), [payload]);

  // Unmount only (`attempt` is stable). Declared before the debounce effect so its cleanup runs
  // first (effects tear down in declaration order); a pending timer means the latest edit was never
  // sent.
  useEffect(
    () => () => {
      // An in-flight save's success handler already chains the pending edit; firing again races it
      // with a stale version token.
      if (timerRef.current != null && pendingRef.current != null && inFlightRef.current == null) {
        clearTimer(timerRef);
        attempt(pendingRef.current);
      }
    },
    [attempt],
  );

  // Adopts the payload from the commit that bumped `cleanToken`; the caller's rehydration setStates
  // have batched by then, so `payloadRef.current` is the freshly loaded value.
  useEffect(() => {
    if (cleanToken === 0) return;
    savedSerializedRef.current = JSON.stringify(payloadRef.current);
  }, [cleanToken]);

  useEffect(() => {
    if (!enabled || conflictRef.current) return;
    if (serialized === savedSerializedRef.current) return;
    pendingRef.current = payload;
    // Edit queued but not sent: "Saved" would be a lie until the timer fires.
    setStatus((s) => (s === "saved" ? "idle" : s));
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = setTimeout(function fire() {
      timerRef.current = null;
      // A superseded save (markClean/clearConflict mid-flight) chains nothing; re-check so this
      // edit is not stranded.
      if (inFlightRef.current != null) {
        timerRef.current = setTimeout(fire, debounceMs);
        return;
      }
      if (pendingRef.current != null) attempt(pendingRef.current);
    }, debounceMs);
    return () => clearTimer(timerRef);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- keyed on the serialized payload
  }, [enabled, serialized, debounceMs, attempt]);

  // Auto-retry only offline failures when connectivity returns; real server rejections wait for
  // manual retry.
  useEffect(() => {
    function handleOnline() {
      if (
        status === "offline" &&
        pendingRef.current != null &&
        !conflictRef.current &&
        inFlightRef.current == null
      ) {
        attempt(pendingRef.current);
      }
    }
    window.addEventListener("online", handleOnline);
    return () => window.removeEventListener("online", handleOnline);
  }, [status, attempt]);

  const retry = useCallback(() => {
    if (conflictRef.current || inFlightRef.current != null) return;
    if (pendingRef.current != null) attempt(pendingRef.current);
  }, [attempt]);

  const forceSave = useCallback(async (): Promise<AutosaveOutcome> => {
    if (conflictRef.current) return "conflict";
    // Clear the debounce timer first or it can fire mid-await and start a redundant save.
    clearTimer(timerRef);
    if (inFlightRef.current) await inFlightRef.current;
    if (conflictRef.current) return "conflict";
    if (JSON.stringify(payloadRef.current) === savedSerializedRef.current) return "saved";
    return attempt(payloadRef.current);
  }, [attempt]);

  // Synchronous (reads refs) so `beforeunload` sees an edit in the debounce window, which `status`
  // misses.
  const hasUnflushedChanges = useCallback(
    () => timerRef.current != null || inFlightRef.current != null || pendingRef.current != null,
    [],
  );

  return {
    status,
    hasUnpersistedEdits: status !== "idle" && status !== "saved",
    retry,
    clearConflict,
    forceSave,
    markClean,
    hasUnflushedChanges,
  };
}
