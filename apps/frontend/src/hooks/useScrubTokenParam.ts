import { useEffect, useState } from "react";
import { useSearchParams } from "react-router";

/** Reads a single-use credential out of the query string once on mount, then strips it from the
 * address bar/history so it never lingers where anything reading the current URL (browser
 * history, a copy-pasted link) could pick it back up. Returns the captured value — every
 * caller needs the token itself, not the (now-scrubbed) URL it arrived in. */
export function useScrubTokenParam(name: string): string {
  const [params, setSearchParams] = useSearchParams();
  const [value] = useState(() => params.get(name) ?? "");

  useEffect(() => {
    if (params.has(name)) {
      setSearchParams(
        (prev) => {
          prev.delete(name);
          return prev;
        },
        { replace: true },
      );
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return value;
}
