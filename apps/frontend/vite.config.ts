import path from 'node:path'
import { defineConfig, searchForWorkspaceRoot, type Plugin } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

const extensionEntry = process.env.ARCODASH_EXT ? path.resolve(process.cwd(), process.env.ARCODASH_EXT) : undefined

// Scan an extension's sources for Tailwind classes.
const extensionSource = (): Plugin => ({
  name: 'extension-tailwind-source',
  enforce: 'pre',
  transform(code, id) {
    if (!extensionEntry || id.split('?')[0] !== path.resolve(__dirname, 'src/index.css')) return
    return `${code}\n@source "${path.dirname(extensionEntry)}";\n`
  },
})

// https://vite.dev/config/
export default defineConfig({
  plugins: [react(), extensionSource(), tailwindcss()],
  // react-grid-layout's deps (react-draggable/react-resizable) read process.env.NODE_ENV
  // at the top level; Vite doesn't polyfill `process` in the browser, so without this
  // define it throws `ReferenceError: process is not defined` on the first drag/resize.
  define: {
    'process.env.NODE_ENV': JSON.stringify(process.env.NODE_ENV ?? 'development'),
  },
  resolve: {
    alias: {
      // '@/extensions' must precede '@' - alias matching is order-sensitive (a plain '@' entry
      // would prefix-match '@/extensions' first and always resolve to src/extensions).
      '@/extensions': extensionEntry ?? path.resolve(__dirname, './src/extensions/index.ts'),
      '@': path.resolve(__dirname, './src'),
    },
    // An extension outside this package must not pull a second copy of these.
    dedupe: ['react', 'react-dom', 'react-router', '@tanstack/react-query', '@codemirror/state', '@codemirror/view'],
  },
  server: {
    port: 3000,
    // An extension outside the workspace must be servable by the dev server.
    fs: {
      allow: [searchForWorkspaceRoot(process.cwd()), ...(extensionEntry ? [path.dirname(extensionEntry)] : [])],
    },
    proxy: {
      // Proxy /api/* to the backend so both share the same origin in dev.
      // This means CSRF cookies set by the backend are readable by frontend JS.
      '/api': {
        target: process.env.BACKEND_URL ?? 'http://localhost:8080',
        changeOrigin: true,
      },
      '/health': {
        target: process.env.BACKEND_URL ?? 'http://localhost:8080',
        changeOrigin: true,
      },
    },
  },
})
