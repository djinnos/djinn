import { cleanup, render, type RenderOptions } from "@testing-library/react"
import { QueryClient, QueryClientProvider } from "@tanstack/react-query"
import { MemoryRouter, type MemoryRouterProps } from "react-router-dom"
import { type ReactElement, type ReactNode } from "react"
import { afterEach } from "vitest"

afterEach(() => {
  cleanup()
})

interface WrapperOptions {
  routerProps?: MemoryRouterProps
}

function createWrapper({ routerProps }: WrapperOptions = {}) {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  })

  return function Wrapper({ children }: { children: ReactNode }) {
    return (
      <QueryClientProvider client={queryClient}>
        <MemoryRouter {...routerProps}>{children}</MemoryRouter>
      </QueryClientProvider>
    )
  }
}

function customRender(
  ui: ReactElement,
  options?: Omit<RenderOptions, "wrapper"> & { wrapperOptions?: WrapperOptions },
) {
  const { wrapperOptions, ...renderOptions } = options ?? {}
  return render(ui, {
    wrapper: createWrapper(wrapperOptions),
    ...renderOptions,
  })
}

export { customRender as render }
export { default as userEvent } from "@testing-library/user-event"
export * from "@testing-library/react"
