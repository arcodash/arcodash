import { useEffect } from "react";

/** Sets `document.title` for as long as the caller is mounted, restoring whatever it was before
 * on unmount — so a page nested inside another (a dialog, a route that unmounts out of order)
 * never leaves a stale title behind. The single mechanism every route uses, so tab/history/
 * bookmark titles share one format instead of each page inventing its own. */
export function useDocumentTitle(title: string): void {
  useEffect(() => {
    const previous = document.title;
    document.title = title;
    return () => {
      document.title = previous;
    };
  }, [title]);
}
