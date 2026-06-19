import { beforeEach, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { GraphToolbar } from "./GraphToolbar";
import { useCodeGraphStore } from "@/stores/codeGraphStore";

describe("GraphToolbar layout mode toggle", () => {
  beforeEach(() => {
    useCodeGraphStore.getState().reset();
  });

  it("renders the layout segmented control with force selected by default", () => {
    render(<GraphToolbar />);

    expect(screen.getByTestId("layout-mode-toggle")).toBeInTheDocument();
    expect(screen.getByTestId("layout-mode-force")).toHaveTextContent("Force");
    expect(screen.getByTestId("layout-mode-sequential")).toHaveTextContent(
      "Sequential",
    );
    expect(screen.getByTestId("layout-mode-radial")).toHaveTextContent(
      "Radial",
    );
    expect(screen.getByTestId("layout-mode-force")).toHaveAttribute(
      "aria-checked",
      "true",
    );
  });

  it("updates the store when a layout mode is clicked", async () => {
    useCodeGraphStore.getState().setGraphReady(true);
    const user = userEvent.setup();
    render(<GraphToolbar />);

    await user.click(screen.getByTestId("layout-mode-sequential"));
    expect(useCodeGraphStore.getState().layoutMode).toBe("sequential");
    expect(screen.getByTestId("layout-mode-sequential")).toHaveAttribute(
      "aria-checked",
      "true",
    );

    await user.click(screen.getByTestId("layout-mode-radial"));
    expect(useCodeGraphStore.getState().layoutMode).toBe("radial");
    expect(screen.getByTestId("layout-mode-radial")).toHaveAttribute(
      "aria-checked",
      "true",
    );
  });

  it("disables all layout mode buttons when the graph is not ready", () => {
    useCodeGraphStore.getState().setGraphReady(false);
    render(<GraphToolbar />);

    expect(screen.getByTestId("layout-mode-force")).toBeDisabled();
    expect(screen.getByTestId("layout-mode-sequential")).toBeDisabled();
    expect(screen.getByTestId("layout-mode-radial")).toBeDisabled();
  });

  it("enables all layout mode buttons when the graph is ready", () => {
    useCodeGraphStore.getState().setGraphReady(true);
    render(<GraphToolbar />);

    expect(screen.getByTestId("layout-mode-force")).not.toBeDisabled();
    expect(screen.getByTestId("layout-mode-sequential")).not.toBeDisabled();
    expect(screen.getByTestId("layout-mode-radial")).not.toBeDisabled();
  });
});
