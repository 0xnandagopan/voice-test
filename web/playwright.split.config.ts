import { defineConfig } from "@playwright/test";
const port = process.env.V0_SPLIT_WEB_TEST_PORT ?? "5798";
const apiPort = process.env.V0_SPLIT_API_TEST_PORT ?? "5799";
export default defineConfig({
  testDir: "./tests",
  testMatch: "split-origin.spec.ts",
  workers: 1,
  retries: 0,
  reporter: "list",
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    headless: true,
    launchOptions: {
      executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE,
    },
  },
  webServer: {
    command: `npm run dev -- --port ${port} --strictPort`,
    url: `http://127.0.0.1:${port}`,
    env: { VITE_API_ORIGIN: `http://127.0.0.1:${apiPort}` },
    reuseExistingServer: false,
  },
  outputDir: "test-results/split-origin",
});
