/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_API_BASE_URL: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

interface Window {
  /** In-flight `/api/session` request kicked off by an inline script in `index.html`'s head,
   * before the entry chunk downloads. `api.getSession()` consumes it once. */
  __SESSION_PRELOAD__?: Promise<Response>;
}
