import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig(({ mode }) => ({
  // Vitest runs Vite in serve mode; disabling Solid HMR there avoids loading
  // the browser-only virtual refresh module during component tests.
  plugins: [solid({ hot: mode !== "test" })],
  server: {
    port: 1420,
    strictPort: true
  },
  test: {
    // Release tooling uses Node's test runner. Keep those files out of Vitest
    // so `npm test` does not collect them as empty suites.
    exclude: [
      "**/node_modules/**",
      "**/dist/**",
      "**/cypress/**",
      "**/.{idea,git,cache,output,temp}/**",
      "**/{karma,rollup,webpack,vite,vitest,jest,ava,babel,nyc,cypress,tsup,build,eslint,prettier}.config.*",
      "scripts/**/*.test.mjs"
    ]
  }
}));
