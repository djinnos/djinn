import { describe, expect, it } from "vitest";
import { render, screen } from "@/test/test-utils";

import { HtmlBlock } from "./HtmlBlock";

describe("HtmlBlock", () => {
  it("renders a sandboxed iframe (no scripts, no same-origin)", () => {
    render(
      <HtmlBlock id="h1" attributes={{}}>
        {"<div><strong>Hello</strong></div>"}
      </HtmlBlock>,
    );
    const iframe = screen.getByTitle("Embedded HTML block") as HTMLIFrameElement;
    expect(iframe.tagName).toBe("IFRAME");
    // Fully locked-down sandbox.
    expect(iframe.getAttribute("sandbox")).toBe("");
    expect(iframe.getAttribute("sandbox")).not.toContain("allow-scripts");
  });

  it("embeds sanitized content in the iframe srcdoc, stripping scripts", () => {
    render(
      <HtmlBlock id="h2" attributes={{}}>
        {"<b>keep</b><script>alert(1)</script>"}
      </HtmlBlock>,
    );
    const iframe = screen.getByTitle("Embedded HTML block") as HTMLIFrameElement;
    const srcdoc = iframe.getAttribute("srcdoc") ?? "";
    expect(srcdoc).toContain("<b>keep</b>");
    expect(srcdoc).not.toContain("alert(1)");
    expect(srcdoc).toContain("Content-Security-Policy");
  });

  it("renders the HTML block shell label", () => {
    render(
      <HtmlBlock id="h3" attributes={{}}>
        {"<p>body</p>"}
      </HtmlBlock>,
    );
    expect(screen.getByText("HTML")).toBeInTheDocument();
  });
});
