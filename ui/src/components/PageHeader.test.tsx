import { describe, it, expect } from "vitest";
import { render, screen } from "@/test/test-utils";
import { PageHeader } from "./PageHeader";

describe("PageHeader", () => {
  it("renders title", () => {
    render(<PageHeader title="Agents" />);
    expect(
      screen.getByRole("heading", { level: 1, name: "Agents" }),
    ).toBeDefined();
  });

  it("renders subtitle when provided", () => {
    render(<PageHeader title="Agents" subtitle="Showing all agents" />);
    expect(screen.getByText("Showing all agents")).toBeDefined();
  });

  it("renders leading element when provided", () => {
    render(<PageHeader title="Detail" leading={<button>Back</button>} />);
    expect(screen.getByRole("button", { name: "Back" })).toBeDefined();
  });

  it("renders actions when provided", () => {
    render(<PageHeader title="Agents" actions={<button>New Agent</button>} />);
    expect(screen.getByRole("button", { name: "New Agent" })).toBeDefined();
  });

  it("does not render subtitle area when subtitle is omitted", () => {
    const { container } = render(<PageHeader title="Agents" />);
    const paragraphs = container.querySelectorAll("p");
    expect(paragraphs.length).toBe(0);
  });

  it("renders children below the header row", () => {
    render(
      <PageHeader title="Agents">
        <p data-testid="child-content">Child content</p>
      </PageHeader>,
    );
    expect(screen.getByTestId("child-content").textContent).toBe(
      "Child content",
    );
  });

  it("applies custom className", () => {
    const { container } = render(
      <PageHeader title="Agents" className="custom-class" />,
    );
    expect(container.firstElementChild!.className).toContain("custom-class");
  });
});
