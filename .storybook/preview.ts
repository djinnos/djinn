import type { Preview } from "@storybook/react"

import "../src/index.css"

document.documentElement.classList.add("dark")

const preview: Preview = {
  parameters: {
    backgrounds: {
      default: "app-dark",
      values: [
        { name: "app-dark", value: "#09090b" },
      ],
    },
  },
}

export default preview
