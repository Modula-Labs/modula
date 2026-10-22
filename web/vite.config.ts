/// <reference types="vitest/config" />
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwind from "@tailwindcss/vite";

// In dev, run modula-server with MODULA_PUBLIC_URL=http://localhost:5173 so the
// browser's Origin matches; the proxy keeps the Origin header untouched.
export default defineConfig({
  plugins: [react(), tailwind()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  server: {
    port: 5173,
    strictPort: true,
    proxy: { "/api": { target: "http://127.0.0.1:7100", changeOrigin: false } },
  },
  build: {
    outDir: "dist",
    sourcemap: false,
    // No inline scripts: the server's CSP is `script-src 'self'`.
    modulePreload: { polyfill: false },
    assetsInlineLimit: 0,
  },
  test: {
    environment: "jsdom",
    globals: true,
  },
});
