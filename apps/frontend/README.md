# @arcodash/frontend

The arcodash React frontend, part of the [arcodash](../../README.md) monorepo. See the root
README and [`docs/`](../../docs) for the full project picture (features, connectors, architecture).

## Stack

React 19, TypeScript, Vite 8 with `@vitejs/plugin-react` (Babel-based, not the SWC variant),
Tailwind CSS 4 via `@tailwindcss/vite`. Data fetching/caching is TanStack Query, tables are
TanStack Table, forms are React Hook Form + Zod, charts are Recharts, the SQL editor is
CodeMirror 6.

## Dev commands

```bash
pnpm dev            # Vite dev server, http://localhost:3000, proxies /api and /health to the backend
pnpm build          # tsc -b && vite build
pnpm typecheck      # tsc -b
pnpm lint           # oxlint
pnpm test           # vitest run
pnpm test:watch     # vitest, watch mode
pnpm test:coverage  # vitest run --coverage
pnpm preview        # preview a production build
```

`vite.config.ts` proxies `/api` and `/health` to `BACKEND_URL` (default `http://localhost:8080`)
so frontend and backend share an origin in dev, and CSRF cookies set by the backend are readable
by frontend JS.

## Testing

Vitest + Testing Library + MSW. `src/__tests__/setup.ts` wires up `@testing-library/jest-dom` and
starts the MSW server (`src/__tests__/msw-server.ts`, handlers in `src/__tests__/handlers.ts`)
around each test run. Run it with `pnpm --filter @arcodash/frontend test`. See
[`docs/guide/testing.md`](../../docs/guide/testing.md) for the full test stack.

## Where things live

- `src/pages/`, one file per route (dashboards, queries, data sources, monitors, admin, auth)
- `src/components/`, shared UI, plus `dashboard/`, `data-sources/`, `query-editor/`,
  `visualizations/` for feature-specific pieces
- `src/lib/api.ts`, the API client; `src/lib/auth.tsx` and `src/lib/jobs.ts` for auth state and
  async query-job polling
- `src/router.tsx`, the route table
