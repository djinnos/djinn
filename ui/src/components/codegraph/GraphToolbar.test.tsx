import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { useCodeGraphStore } from "@/stores/codeGraphStore";
import { GraphToolbar } from "./GraphToolbar";

describe("GraphToolbar", () => {
  beforeEach(() => {
    useCodeGraphStore.getState().reset();
  });

  it("renders the lens selector with architecture selected by default", () => {
    render(<GraphToolbar />);

    const lensSelector = screen.getByTestId("lens-selector");
    expect(lensSelector).toBeInTheDocument();

    const archButton = screen.getByTestId("lens-architecture");
    expect(archButton).toHaveAttribute("aria-checked", "true");

    const callsButton = screen.getByTestId("lens-calls");
    expect(callsButton).toHaveAttribute("aria-checked", "false");
  });

  it("clicking a lens button calls applyLens", async () => {
    const user = userEvent.setup();
    render(<GraphToolbar />);

    await user.click(screen.getByTestId("lens-calls"));

    expect(useCodeGraphStore.getState().activeLens).toBe("calls");
    expect(useCodeGraphStore.getState().edgeKindFilters.Defines).toBe(true);
    expect(useCodeGraphStore.getState().edgeKindFilters.SymbolReference).toBe(
      true,
    );
    expect(useCodeGraphStore.getState().edgeKindFilters.Implements).toBe(false);
    expect(useCodeGraphStore.getState().nodeKindFilters.folder).toBe(false);
    expect(useCodeGraphStore.getState().nodeKindFilters.symbol).toBe(true);
  });

  it("renders an Advanced disclosure containing the raw toggle groups", () => {
    render(<GraphToolbar />);

    const advanced = screen.getByText("Advanced");
    expect(advanced).toBeInTheDocument();

    // The <details> should be collapsed by default
    const details = advanced.closest("details");
    expect(details).not.toBeNull();
    expect(details).not.toHaveAttribute("open");

    // The raw toggles are inside the details
    const nodeToggle = screen.getByTestId("node-filter-folder");
    expect(details!.contains(nodeToggle)).toBe(true);

    const edgeToggle = screen.getByTestId("edge-filter-Defines");
    expect(details!.contains(edgeToggle)).toBe(true);
  });

  it("renders Tests, Zoom, Color, and DOI controls outside the Advanced disclosure", () => {
    render(<GraphToolbar />);

    const advanced = screen.getByText("Advanced");
    const details = advanced.closest("details")!;

    const testsToggle = screen.getByTestId("tests-hide-toggle");
    expect(details.contains(testsToggle)).toBe(false);

    const zoomToggle = screen.getByTestId("semantic-zoom-toggle");
    expect(details.contains(zoomToggle)).toBe(false);

    const colorToggle = screen.getByTestId("color-mode-toggle");
    expect(details.contains(colorToggle)).toBe(false);

    const doiControl = screen.getByTestId("doi-reveal-control");
    expect(details.contains(doiControl)).toBe(false);
  });
});
