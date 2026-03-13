import { describe, expect, it } from "vitest";
import { EpicCard } from "./EpicCard";
import { render, screen } from "@/test/test-utils";
import { mockEpicB, mockTaskA, mockTaskB } from "@/test/fixtures";

describe("EpicCard", () => {
  it("renders title, task counts, emoji, and progress", () => {
    const epic = {
      ...mockEpicB,
      title: "Payments Revamp",
    };

    render(
      <EpicCard
        epic={epic}
        emoji="🚀"
        mockTasks={[{ ...mockTaskA, status: "closed" }, { ...mockTaskB, status: "open" }]}
      />,
    );

    expect(screen.getByText(epic.title)).toBeInTheDocument();
    expect(screen.getByText("1 / 2 done")).toBeInTheDocument();
    expect(screen.getByLabelText("epic emoji")).toHaveTextContent("🚀");
    expect(screen.getByText(/50%/)).toBeInTheDocument();
  });
});
