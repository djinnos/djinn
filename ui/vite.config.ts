import path from "path"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: [
      { find: "@", replacement: path.resolve(__dirname, "./src") },
      // elkjs's main entry requires('web-worker') for Node — point the bare
      // specifier at the browser bundle instead. Use an EXACT (anchored) match:
      // a plain string alias matches as a prefix, so it would also rewrite
      // subpath imports like `elkjs/lib/elk.bundled.js` (used by
      // beautiful-mermaid) into a doubled `.../elk.bundled.js/lib/elk.bundled.js`
      // path that the production rollup/commonjs resolver fails to load.
      {
        find: /^elkjs$/,
        replacement: path.resolve(
          __dirname,
          "./node_modules/elkjs/lib/elk.bundled.js",
        ),
      },
    ],
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
