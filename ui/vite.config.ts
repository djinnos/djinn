import path from "path"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
      // elkjs main entry requires('web-worker') for Node — use browser bundle
      "elkjs": path.resolve(__dirname, "./node_modules/elkjs/lib/elk.bundled.js"),
    },
  },
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/.djinn/**"],
    },
  },
  // Absolute base so hashed-asset URLs in index.html are root-anchored
  // (`/assets/...`). With a relative base (`./`), a hard-refresh on a nested
  // route like /proposals/<id> makes the browser resolve assets against
  // /proposals/, request paths that don't exist, and get index.html (HTML)
  // back where it expected JS — a white page.
  base: '/',
  build: {
    target: "chrome130",
    minify: true,
    sourcemap: false,
  },
})
