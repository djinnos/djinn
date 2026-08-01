import { Route, Routes } from "react-router-dom";

import { render, screen, waitFor } from "@/test/test-utils";
import { projectStore } from "@/stores/projectStore";
import type { ReadinessRunDetail } from "@/api/readiness";
import { ProjectReadinessPage } from "./ProjectReadinessPage";
import terminalDetailFixture from "./fixtures/readiness_terminal_detail.json";

vi.mock("@/components/AuthGate", () => ({
  useAuthUser: () => ({ id: "owner-1", login: "readiness-owner", name: "Readiness Owner", avatarUrl: null, isAdmin: false, role: "engineer" }),
}));

const snapshot = "d34db33fd34db33fd34db33fd34db33fd34db33f";
// Shared with the authenticated Axum regression. This is an HTTP DTO, rather
// than a component model, so the routed page consumes the server wire shape.
const terminalDetail: ReadinessRunDetail = terminalDetailFixture;
const run = terminalDetail.run;

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
    // The frozen composition is rendered whole, exactly as the route serializes it.
    expect(screen.getByRole("article", { name: "Readiness area frontend" })).toHaveTextContent('{"roles":["frontend"],"evidence":["web/package.json"],"languages":["TypeScript"],"confidence":0.95,"frameworks":["React"],"key_libraries":["zod"]}');
    expect(screen.getByRole("article", { name: "Readiness area frontend" })).toHaveTextContent("Persisted area score: 0.8");
    expect(screen.getByRole("article", { name: "Readiness area backend" })).toHaveTextContent('{"roles":["backend"],"evidence":["server/Cargo.toml"],"languages":["Rust"],"confidence":0.97,"frameworks":["Axum"],"key_libraries":["sqlx"]}');
    expect(screen.getByRole("article", { name: "Readiness area backend" })).toHaveTextContent("Persisted area score: 0.75");
    expect(screen.getByTestId("current-attempt-area-frontend")).toHaveTextContent("#1 — succeeded");
    expect(screen.getByTestId("current-attempt-area-backend")).toHaveTextContent("#1 — succeeded");
    expect(screen.getByText("frontend-auth")).toBeInTheDocument();
    expect(screen.getByText("Confidence: 0.95")).toBeInTheDocument();
    expect(screen.getByText('Evidence: [{"line":12,"path":"web/auth.ts"}]')).toBeInTheDocument();
    expect(screen.getByText("backend-auth")).toBeInTheDocument();
    expect(screen.getByText("Confidence: 0.9")).toBeInTheDocument();
    expect(screen.getByText('Evidence: [{"line":48,"path":"server/src/auth.rs"}]')).toBeInTheDocument();
    expect(screen.getByText("backend-secrets")).toBeInTheDocument();
    expect(screen.getByText("Confidence: 0.85")).toBeInTheDocument();
    expect(screen.getByText('Evidence: [{"line":9,"path":"server/src/config.rs"}]')).toBeInTheDocument();
    expect(screen.getByLabelText("frontend Unsupported entries")).toHaveTextContent("browser-session");
    expect(screen.getByLabelText("frontend Warnings")).toHaveTextContent("legacy form remains outside migration scope");
    expect(screen.getByLabelText("backend Warnings")).toHaveTextContent("secret rotation is not configured");
    // The validator-accepted backend result carries no out-of-contract `errors`
    // array; its degraded guardrail surfaces as the analysis_error finding and
    // as the run-level `completed_with_errors` aggregate below.
    expect(screen.queryByLabelText("backend Errors")).toBeNull();
    expect(screen.getByLabelText("Run diagnostics")).toHaveTextContent('readiness_aggregated: {"band":"ready","owner":"authenticated-http-fixture","score":0.7777777777777778,"status":"completed_with_errors"}');
    expect(screen.getByLabelText("Suggested next actions")).toHaveTextContent("Verify web/auth.ts and server/src/auth.rs after the change.");
    expect(screen.getAllByText("shared-auth-remediation")).toHaveLength(1);

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(fetchMock.mock.calls[0][0]).toContain("/api/projects/project-readiness/readiness");
    expect(fetchMock.mock.calls[1][0]).toContain("/api/projects/project-readiness/readiness/run-terminal");
  });
});
