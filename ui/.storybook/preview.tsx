import type { Preview } from "@storybook/react-vite"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"

import "../src/styles/globals.css"

document.documentElement.classList.add("dark")

// Global react-query client so any story rendering components that call
// useQuery works without a per-story provider. Retries and focus refetches
// are disabled so queries against the nonexistent Storybook API fail once
// and settle into their error/empty states instead of retry-spinning.
// Stories that build their own QueryClientProvider (e.g. to seed data)
// still win — the nearest provider takes precedence.
const queryClient = new QueryClient({
  defaultOptions: {
    queries: { retry: false, refetchOnWindowFocus: false, staleTime: Infinity },
  },
})

const preview: Preview = {
  parameters: {
    backgrounds: {
      options: {
        "app-dark": { name: "app-dark", value: "#09090b" }
      }
    },
  },

  decorators: [
    (Story) => (
      <QueryClientProvider client={queryClient}>
        <Story />
      </QueryClientProvider>
    ),
  ],

  initialGlobals: {
    backgrounds: {
      value: "app-dark"
    }
  }
}

export default preview
