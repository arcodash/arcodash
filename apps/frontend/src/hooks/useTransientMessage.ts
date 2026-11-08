import { useCallback, useEffect, useRef, useState } from "react";

/** A message that clears itself after `ms`. Re-setting it restarts the window rather than
 * stacking a second timer, and the timer is dropped on unmount. */
export function useTransientMessage<T>(ms = 5000): [T | null, (value: T | null) => void] {
  const [value, setValue] = useState<T | null>(null);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const show = useCallback(
    (next: T | null) => {
      if (timerRef.current) clearTimeout(timerRef.current);
      setValue(next);
      if (next != null) timerRef.current = setTimeout(() => setValue(null), ms);
    },
    [ms],
  );

  useEffect(() => () => {
    if (timerRef.current) clearTimeout(timerRef.current);
  }, []);

  return [value, show];
}
