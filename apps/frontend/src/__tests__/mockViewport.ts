/** Overrides `window.matchMedia` to report a given match state, for components (like
 * `useMediaQuery`/`useListView`) that branch on real JS matchMedia rather than a CSS `@media`
 * query jsdom can't evaluate. `setup.ts` stubs `matches: false` globally, so anything gated on
 * `(min-width: …)` reads as "mobile" by default — call `mockViewport(true)` in a test/describe
 * that needs the desktop branch, and restore it afterwards. */
export function mockViewport(matches: boolean): () => void {
  const original = window.matchMedia;
  window.matchMedia = ((query: string) => ({
    matches,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
  return () => {
    window.matchMedia = original;
  };
}
