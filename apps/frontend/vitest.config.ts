import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import { resolve } from "path";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    setupFiles: ["src/__tests__/setup.ts"],
    globals: true,
    // Default 5s trips on multi-step UI flows when the machine is CPU-starved.
    testTimeout: 15000,
    env: {
      // Fixed regardless of .env.local so MSW handlers (hardcoded to this URL)
      // match identically in CI and locally.
      VITE_API_BASE_URL: "http://localhost:8080",
    },
    coverage: {
      provider: "v8",
      reporter: ["text", "html"],
      exclude: ["src/__tests__/**", "src/main.tsx"],
    },
  },
  resolve: {
    alias: {
      "@": resolve(__dirname, "src"),
    },
  },
});
