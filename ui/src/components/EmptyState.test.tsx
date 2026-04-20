import { describe, it, expect, vi } from "vitest"
import { render, screen } from "@/test/test-utils"
import { EmptyState } from "@/components/EmptyState"

describe("EmptyState", () => {
  it("renders title, message, and action button", () => {
    const onAction = vi.fn()

    render(
      <EmptyState
        title="No items yet"
        message="Create your first item to get started."
        actionLabel="Create item"
        onAction={onAction}
      />,
    )

    expect(screen.getByText("No items yet")).toBeInTheDocument()
    expect(screen.getByText("Create your first item to get started.")).toBeInTheDocument()
    expect(screen.getByRole("button", { name: "Create item" })).toBeInTheDocument()
  })
})
