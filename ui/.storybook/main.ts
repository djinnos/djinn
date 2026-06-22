import type { StorybookConfig } from "@storybook/react-vite"

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
  }
}

export default config
