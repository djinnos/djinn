import { defineConfig, mergeConfig } from "vitest/config"
import viteConfig from "./vite.config"

export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      globals: true,
      environment: "jsdom",
      setupFiles: ["./src/test/setup.ts"],
      css: false,
      include: ["src/**/*.test.{ts,tsx}"],
      exclude: ["node_modules", ".cache", ".djinn", "dist"],
      maxWorkers: 2,
      poolOptions: {
        vmThreads: {
          memoryLimit: "512MB",
        },
      },
      passWithNoTests: true,
    },
  }),
)
