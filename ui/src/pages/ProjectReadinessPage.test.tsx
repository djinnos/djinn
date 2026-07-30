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
    expect(screen.getByRole("article", { name: "Readiness area frontend" })).toHaveTextContent('"languages":["TypeScript"],"roles":["frontend"]');
    expect(screen.getByRole("article", { name: "Readiness area frontend" })).toHaveTextContent("Persisted area score: 0.8");
    expect(screen.getByRole("article", { name: "Readiness area backend" })).toHaveTextContent('"languages":["Rust"],"roles":["backend"]');
    expect(screen.getByRole("article", { name: "Readiness area backend" })).toHaveTextContent("Persisted area score: 0.75");
    expect(screen.getByTestId("current-attempt-area-frontend")).toHaveTextContent("#1 — succeeded");
    expect(screen.getByTestId("current-attempt-area-backend")).toHaveTextContent("#1 — succeeded");
    expect(screen.getByText("frontend-auth")).toBeInTheDocument();
    expect(screen.getByText("Confidence: 0.95")).toBeInTheDocument();
    expect(screen.getByText('Evidence: [{"path":"web/auth.ts","line":12}]')).toBeInTheDocument();
    expect(screen.getByText("backend-auth")).toBeInTheDocument();
    expect(screen.getByText("Confidence: 0.9")).toBeInTheDocument();
    expect(screen.getByText('Evidence: [{"path":"server/src/auth.rs","line":48}]')).toBeInTheDocument();
    expect(screen.getByText("backend-secrets")).toBeInTheDocument();
    expect(screen.getByText("Confidence: 0.85")).toBeInTheDocument();
    expect(screen.getByText('Evidence: [{"path":"server/src/config.rs","line":9}]')).toBeInTheDocument();
    expect(screen.getByLabelText("frontend Unsupported entries")).toHaveTextContent("browser-session");
    expect(screen.getByLabelText("frontend Warnings")).toHaveTextContent("legacy form remains outside migration scope");
    expect(screen.getByLabelText("backend Warnings")).toHaveTextContent("secret rotation is not configured");
    expect(screen.getByLabelText("backend Errors")).toHaveTextContent("secret rotation is not configured");
    expect(screen.getByLabelText("Run diagnostics")).toHaveTextContent("readiness_aggregated");
    expect(screen.getByLabelText("Suggested next actions")).toHaveTextContent("Verify web/auth.ts and server/src/auth.rs after the change.");
    expect(screen.getAllByText("shared-auth-remediation")).toHaveLength(1);

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(fetchMock.mock.calls[0][0]).toContain("/api/projects/project-readiness/readiness");
    expect(fetchMock.mock.calls[1][0]).toContain("/api/projects/project-readiness/readiness/run-terminal");
  });
});
