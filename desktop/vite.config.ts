import path from "path"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      "@tauri-apps/api/core": path.resolve(__dirname, "./src/electron/shims/tauri-core.ts"),
      "@tauri-apps/api/event": path.resolve(__dirname, "./src/electron/shims/tauri-event.ts"),
      "@tauri-apps/api/window": path.resolve(__dirname, "./src/electron/shims/tauri-window.ts"),
      "@tauri-apps/plugin-opener": path.resolve(__dirname, "./src/electron/shims/tauri-opener.ts"),
      // elkjs main entry requires('web-worker') for Node — use browser bundle
      "elkjs": path.resolve(__dirname, "./node_modules/elkjs/lib/elk.bundled.js"),
    },
  },
  // Vite options tailored for Tauri development
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/.djinn/**"],
    },
  },
  base: './',
  build: {
    target: "chrome130",
    minify: true,
    sourcemap: false,
  },
})
