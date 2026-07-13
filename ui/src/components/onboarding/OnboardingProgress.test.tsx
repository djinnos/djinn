import { describe, expect, it } from "vitest";
import { render, screen } from "@/test/test-utils";

import { OnboardingProgress } from "./OnboardingProgress";

describe("OnboardingProgress", () => {
  it("marks the active step and keeps all step numbers available on narrow screens", () => {
    const { rerender } = render(<OnboardingProgress current="models" />);

    expect(screen.getByLabelText("Onboarding progress")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Step 2 of 3: Models");
    expect(screen.getByText("Models").previousElementSibling).toHaveAttribute(
      "aria-current",
      "step",
    );
    expect(screen.getByText("Environment")).toBeInTheDocument();

    rerender(<OnboardingProgress current="environment" />);
    expect(screen.getByRole("status")).toHaveTextContent(
      "Step 3 of 3: Environment",
    );
  });

  it("marks every step complete on the final confirmation screen", () => {
    render(<OnboardingProgress current="environment" complete />);

    expect(screen.getByRole("status")).toHaveTextContent("Onboarding complete");
    expect(screen.queryByRole("listitem", { current: "step" })).not.toBeInTheDocument();
    expect(screen.getAllByRole("listitem")).toHaveLength(3);
  });
});
