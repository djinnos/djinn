import { describe, expect, it, vi } from "vitest";
import { render, screen, userEvent } from "@/test/test-utils";
import type { ReadinessRunDetail } from "@/api/readiness";
import { ProjectReadinessPanel } from "./ProjectReadinessPanel";

function detail(status = "completed"): ReadinessRunDetail {
  return {
    run: {
      id: "run-1", project_id: "project-1", status, repository_snapshot: "a1b2c3d4",
      skill_name: "readiness-guardrails", skill_version: "2.4.0", expected_area_count: 2,
      created_at: "2026-07-29T12:00:00Z", completed_at: "2026-07-29T12:01:00Z",
    },
    areas: [{
      id: "frontend", area_key: "frontend", status: "succeeded", frozen_at: "2026-07-29T12:00:01Z",
      composition: { languages: ["TypeScript"], frameworks: ["React"] }, path_scopes: ["ui/"],
      attempts: [{ id: "attempt-2", attempt_number: 2, status: "succeeded", payload_digest: "digest-2", created_at: "2026-07-29T12:00:02Z", terminal_at: "2026-07-29T12:00:03Z", is_current: true }],
      accepted_findings: [{ id: "finding-1", attempt_id: "attempt-2", guardrail_key: "frontend-auth", status: "covered", severity: "high", confidence: 0.95, evidence: [{ path: "ui/src/auth.ts", line: 12 }], created_at: "2026-07-29T12:00:03Z" }],
      accepted_outputs: [{ attempt_id: "attempt-2", created_at: "2026-07-29T12:00:03Z", result: {
        unsupported: [{ guardrail_key: "legacy-browser", reason: "not applicable" }],
        warnings: [{ reason: "legacy form remains" }],
        errors: [{ message: "one guardrail timed out" }],
      } }],
    }],
    area_scores: [{ area_id: "frontend", score: 0.8, applicable_weight: 5, covered_weight: 4, status: "completed", created_at: "2026-07-29T12:01:00Z" }],
    project_score: { score: 0.8, band: "ready", created_at: "2026-07-29T12:01:00Z" },
    suggestions: [{ id: "suggestion-1", dedupe_key: "auth-remediation", created_at: "2026-07-29T12:01:00Z", suggestion: { action: "Add CSRF protection", validation_guidance: "Run the authentication integration tests." } }],
    events: [],
  };
}

describe("ProjectReadinessPanel", () => {
  it("renders an empty start affordance and invokes only the injected callback", async () => {
    const onStart = vi.fn();
    render(<ProjectReadinessPanel detail={null} ownerContext="octocat" onStart={onStart} />);

    expect(screen.getByRole("status")).toHaveTextContent("No readiness analysis has started.");
    await userEvent.click(screen.getByRole("button", { name: "Start readiness analysis" }));
    expect(onStart).toHaveBeenCalledOnce();
  });

  it("renders a kickoff pending status", () => {
    render(<ProjectReadinessPanel detail={null} ownerContext="octocat" isStarting />);
    expect(screen.getByRole("status")).toHaveTextContent("Starting readiness analysis");
    expect(screen.queryByRole("button", { name: "Start readiness analysis" })).not.toBeInTheDocument();
  });

  it.each([
    ["identifying", "Identifying repository composition"],
    ["analyzing", "Analyzing readiness areas"],
    ["aggregating", "Aggregating readiness results"],
    ["completed", "Readiness analysis completed"],
    ["completed_with_errors", "Readiness analysis completed with errors"],
  ])("renders %s with a visible accessible status", (status, label) => {
    render(<ProjectReadinessPanel detail={detail(status)} ownerContext="octocat" />);
    expect(screen.getByRole("status")).toHaveTextContent(label);
  });

  it("renders failed state as an accessible error", () => {
    render(<ProjectReadinessPanel detail={detail("failed")} ownerContext="octocat" />);
    expect(screen.getByRole("alert")).toHaveTextContent("Readiness analysis failed");
  });

  it("renders persisted terminal detail without mutable readiness controls", () => {
    render(<ProjectReadinessPanel detail={detail()} ownerContext="Owner: octocat" />);

    expect(screen.getByText("Score: 0.8 — Band: ready")).toBeInTheDocument();
    expect(screen.getByText("Owner: octocat")).toBeInTheDocument();
    expect(screen.getByText("a1b2c3d4")).toBeInTheDocument();
    expect(screen.getByText("readiness-guardrails v2.4.0")).toBeInTheDocument();
    expect(screen.getByText("Persisted area score: 0.8")).toBeInTheDocument();
    expect(screen.getByTestId("current-attempt-frontend")).toHaveTextContent("#2 — succeeded");
    expect(screen.getByRole("region", { name: "frontend accepted findings" })).toHaveTextContent("frontend-auth");
    expect(screen.getByText(/Evidence:.*ui\/src\/auth.ts/)).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "frontend Unsupported entries" })).toHaveTextContent("legacy-browser");
    expect(screen.getByRole("region", { name: "frontend Warnings" })).toHaveTextContent("legacy form remains");
    expect(screen.getByRole("region", { name: "frontend Errors" })).toHaveTextContent("one guardrail timed out");
    expect(screen.getByRole("region", { name: "Suggested next actions" })).toHaveTextContent("Add CSRF protection");
    expect(screen.getByText("Validation guidance: Run the authentication integration tests.")).toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
  });
});
