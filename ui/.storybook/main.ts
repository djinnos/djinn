import path from "path"
import type { StorybookConfig } from "@storybook/react-vite"

// Modules that must be substituted for browser-runnable mocks under
// `storybook dev`. vitest's `vi.mock(...)` crashes in the Vite preview runtime
// and Storybook 9.1's `sb.mock()` is a no-op stub on the Vite builder, so we
// swap these at bundle time with story-controlled mocks that expose a
// configurable registry (see src/storybook-mocks/*). Regex `find`s are exact
// so only these specifiers are redirected — everything else keeps the real
// `@` → src alias from vite.config.ts. cwd is the Storybook project root (ui/).
const mockAliases = [
  {
    find: /^@\/api\/mcpClient$/,
    replacement: path.resolve(process.cwd(), "src/storybook-mocks/mcpClient.ts"),
  },
  {
    find: /^@\/hooks\/useSessionMessages$/,
    replacement: path.resolve(process.cwd(), "src/storybook-mocks/useSessionMessages.ts"),
  },
]

const config: StorybookConfig = {
  // Only `*.stories.*` — we intentionally do NOT glob `**/*.mdx`: the sole MDX
  // under `src/` is `__fixtures__/canonicalProposal.mdx` (a sample *proposal*
  // body, not a Storybook doc) which fails acorn indexing and 500s the whole
  // story index. Add an explicit glob here if real Storybook MDX docs land.
  stories: ["../src/**/*.stories.@(js|jsx|mjs|ts|tsx)"],
  addons: ["@storybook/addon-links", "@storybook/addon-a11y", "@storybook/addon-docs"],

  framework: {
    name: "@storybook/react-vite",
    options: {},
  },

  viteFinal: async (viteConfig) => {
    viteConfig.resolve ??= {}
    const existing = viteConfig.resolve.alias
    // vite.config.ts declares `alias` in array form; the mock aliases must come
    // FIRST so they win over the broad `@` prefix alias (vite uses first match).
    if (Array.isArray(existing)) {
      viteConfig.resolve.alias = [...mockAliases, ...existing]
    } else {
      const inherited = Object.entries(existing ?? {}).map(([find, replacement]) => ({
        find,
        replacement: replacement as string,
      }))
      viteConfig.resolve.alias = [...mockAliases, ...inherited]
    }
    return viteConfig
  },
}

export default config
