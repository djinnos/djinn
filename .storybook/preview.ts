import type { Preview } from "@storybook/react-vite"

import "../src/styles/globals.css"

document.documentElement.classList.add("dark")

const preview: Preview = {
  parameters: {
    backgrounds: {
      options: {
        "app-dark": { name: "app-dark", value: "#09090b" }
      }
    },
  },

  initialGlobals: {
    backgrounds: {
      value: "app-dark"
    }
  }
}

export default preview
