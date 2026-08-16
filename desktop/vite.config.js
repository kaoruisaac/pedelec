import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig(({ mode }) => ({
  // Vitest runs Vite in serve mode; disabling Solid HMR there avoids loading
  // the browser-only virtual refresh module during component tests.
  plugins: [solid({ hot: mode !== "test" })],
  server: {
    port: 1420,
    strictPort: true
  }
}));
