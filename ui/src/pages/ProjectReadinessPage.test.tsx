import { Route, Routes } from "react-router-dom";

import { render, screen, waitFor } from "@/test/test-utils";
import { projectStore } from "@/stores/projectStore";
import { ProjectReadinessPage } from "./ProjectReadinessPage";

vi.mock("@/components/AuthGate", () => ({
  useAuthUser: () => ({ id: "owner-1", login: "readiness-owner", name: "Readiness Owner", avatarUrl: null, isAdmin: false, role: "engineer" }),
}));

const snapshot = "d34db33fd34db33fd34db33fd34db33fd34db33f";
const run = {
  id: "run-terminal", project_id: "project-readiness", status: "completed_with_errors", repository_snapshot: snapshot,
  skill_name: "agent-readiness-guardrails", skill_version: "1.0.0", expected_area_count: 2,
  created_at: "2026-07-30T12:00:00Z", completed_at: "2026-07-30T12:01:00Z",
};

// This is deliberately the route's serialized detail shape, not a component
// model. The page must consume it solely through the browser HTTP client.
const terminalDetail = {
  run,
  areas: [
    {
      id: "area-frontend", area_key: "frontend", composition: { languages: ["TypeScript"], roles: ["frontend"] }, path_scopes: ["web/"],
      frozen_at: "2026-07-30T12:00:01Z", status: "succeeded",
      attempts: [{ id: "attempt-frontend", attempt_number: 1, status: "succeeded", payload_digest: "frontend-digest", created_at: "2026-07-30T12:00:02Z", terminal_at: "2026-07-30T12:00:03Z", is_current: true }],
      accepted_findings: [{ id: "finding-frontend", attempt_id: "attempt-frontend", guardrail_key: "frontend-auth", status: "covered", severity: "high", confidence: 0.95, evidence: [{ path: "web/auth.ts", line: 12 }], created_at: "2026-07-30T12:00:03Z" }],
      accepted_outputs: [{ attempt_id: "attempt-frontend", result: { unsupported: [{ guardrail_key: "browser-session", reason: "not applicable to this frontend" }], warnings: [{ reason: "legacy form remains outside migration scope" }] }, created_at: "2026-07-30T12:00:03Z" }],
    },
    {
      id: "area-backend", area_key: "backend", composition: { languages: ["Rust"], roles: ["backend"] }, path_scopes: ["server/"],
      frozen_at: "2026-07-30T12:00:01Z", status: "succeeded",
      attempts: [{ id: "attempt-backend", attempt_number: 1, status: "succeeded", payload_digest: "backend-digest", created_at: "2026-07-30T12:00:02Z", terminal_at: "2026-07-30T12:00:03Z", is_current: true }],
      accepted_findings: [{ id: "finding-backend", attempt_id: "attempt-backend", guardrail_key: "backend-secrets", status: "analysis_error", severity: "low", confidence: 0.85, evidence: [{ path: "server/src/config.rs", line: 9 }], created_at: "2026-07-30T12:00:03Z" }],
      accepted_outputs: [{ attempt_id: "attempt-backend", result: { errors: [{ reason: "secret rotation is not configured" }] }, created_at: "2026-07-30T12:00:03Z" }],
    },
  ],
  area_scores: [
    { area_id: "area-frontend", score: 0.8, applicable_weight: 5, covered_weight: 4, status: "supported", created_at: "2026-07-30T12:01:00Z" },
    { area_id: "area-backend", score: 0.75, applicable_weight: 4, covered_weight: 3, status: "supported", created_at: "2026-07-30T12:01:00Z" },
  ],
  project_score: { score: 7 / 9, band: "ready", created_at: "2026-07-30T12:01:00Z" },
  suggestions: [
    { id: "suggestion-1", dedupe_key: "shared-auth-remediation", suggestion: { action: "Apply shared authentication remediation", validation_guidance: "Verify web/auth.ts and server/src/auth.rs after the change." }, created_at: "2026-07-30T12:01:00Z" },
    { id: "suggestion-duplicate", dedupe_key: "shared-auth-remediation", suggestion: { action: "Duplicate must not render" }, created_at: "2026-07-30T12:01:00Z" },
  ],
  events: [{ id: "event-1", event_kind: "aggregation_completed_with_errors", payload: { warnings: ["one guardrail could not be analyzed"] }, created_at: "2026-07-30T12:01:00Z" }],
};

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), { headers: { "Content-Type": "application/json" } });
}

describe("ProjectReadinessPage terminal HTTP detail", () => {
  beforeEach(() => {
    projectStore.setState({ projects: [{ id: "project-readiness", name: "djinnos/readiness-fixture", github_owner: "djinnos", github_repo: "readiness-fixture" }], selectedProjectId: "project-readiness", lastViewPerProject: {} });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    projectStore.setState({ projects: [], selectedProjectId: null, lastViewPerProject: {} });
  });

  it("renders the persisted two-area terminal DTO through the routed HTTP boundary", async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(jsonResponse(run)).mockResolvedValueOnce(jsonResponse(terminalDetail));
    vi.stubGlobal("fetch", fetchMock);

    render(<Routes><Route path="/projects/:id/readiness" element={<ProjectReadinessPage />} /></Routes>, { wrapperOptions: { routerProps: { initialEntries: ["/projects/project-readiness/readiness"] } } });

    await screen.findByTestId("readiness-terminal-detail");
    expect(screen.getByText("Readiness Owner (@readiness-owner)")).toBeInTheDocument();
    expect(screen.getByText(snapshot)).toBeInTheDocument();
    expect(screen.getByText("agent-readiness-guardrails v1.0.0")).toBeInTheDocument();
    expect(screen.getByText("Score: 0.7777777777777778 — Band: ready")).toBeInTheDocument();
    expect(screen.getAllByRole("article", { name: /Readiness area/ })).toHaveLength(2);
    expect(screen.getByRole("article", { name: "Readiness area frontend" })).toHaveTextContent('"languages":["TypeScript"]');
    expect(screen.getByRole("article", { name: "Readiness area frontend" })).toHaveTextContent("Persisted area score: 0.8");
    expect(screen.getByRole("article", { name: "Readiness area backend" })).toHaveTextContent('"languages":["Rust"]');
    expect(screen.getByRole("article", { name: "Readiness area backend" })).toHaveTextContent("Persisted area score: 0.75");
    expect(screen.getByTestId("current-attempt-area-frontend")).toHaveTextContent("#1 — succeeded");
    expect(screen.getByTestId("current-attempt-area-backend")).toHaveTextContent("#1 — succeeded");
    expect(screen.getByText("frontend-auth")).toBeInTheDocument();
    expect(screen.getByText("Confidence: 0.95")).toBeInTheDocument();
    expect(screen.getByText('Evidence: [{"path":"web/auth.ts","line":12}]')).toBeInTheDocument();
    expect(screen.getByText("backend-secrets")).toBeInTheDocument();
    expect(screen.getByText("Confidence: 0.85")).toBeInTheDocument();
    expect(screen.getByText('Evidence: [{"path":"server/src/config.rs","line":9}]')).toBeInTheDocument();
    expect(screen.getByLabelText("frontend Unsupported entries")).toHaveTextContent("browser-session");
    expect(screen.getByLabelText("frontend Warnings")).toHaveTextContent("legacy form remains outside migration scope");
    expect(screen.getByLabelText("backend Errors")).toHaveTextContent("secret rotation is not configured");
    expect(screen.getByLabelText("Run diagnostics")).toHaveTextContent("aggregation_completed_with_errors");
    expect(screen.getByLabelText("Suggested next actions")).toHaveTextContent("Verify web/auth.ts and server/src/auth.rs after the change.");
    expect(screen.getAllByText("shared-auth-remediation")).toHaveLength(1);

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(fetchMock.mock.calls[0][0]).toContain("/api/projects/project-readiness/readiness");
    expect(fetchMock.mock.calls[1][0]).toContain("/api/projects/project-readiness/readiness/run-terminal");
  });
});
