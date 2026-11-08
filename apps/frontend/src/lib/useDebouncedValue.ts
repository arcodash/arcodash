import { useEffect, useState } from "react";

/** Returns `value`, but delayed by `delayMs` after the last change — for debouncing
 * search input into a query key without touching the input's own (immediate) state. */
export function useDebouncedValue<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);

  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(timer);
  }, [value, delayMs]);

  return debounced;
}
