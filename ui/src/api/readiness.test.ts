import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@/api/serverUrl", () => ({ getServerBaseUrl: () => "https://djinn.example.test" }));

import {
  ReadinessHttpError,
  createReadinessKickoffAction,
  fetchActiveOrLatestReadiness,
  fetchReadinessRunDetail,
  kickoffReadiness,
} from "./readiness";

const summary = {
  id: "run-1",
  project_id: "project-1",
  status: "completed",
  repository_snapshot: "abc123",
  skill_name: "agent-readiness-guardrails",
  skill_version: "1.0.0",
  expected_area_count: 1,
  created_at: "2026-07-29T12:00:00Z",
  completed_at: "2026-07-29T12:01:00Z",
};

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

beforeEach(() => vi.unstubAllGlobals());
afterEach(() => vi.unstubAllGlobals());

describe("readiness browser client", () => {
  it("decodes a null active-or-latest state with browser credentials", async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(null));
    vi.stubGlobal("fetch", fetchMock);

    await expect(fetchActiveOrLatestReadiness("project / one")).resolves.toBeNull();

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock.mock.calls[0][0]).toBe(
      "https://djinn.example.test/api/projects/project%20%2F%20one/readiness",
    );
    expect(fetchMock.mock.calls[0][1]).toMatchObject({
      credentials: "include",
      headers: { Accept: "application/json" },
    });
  });

  it("decodes the complete project-scoped detail projection without starting a run", async () => {
    const detail = {
      run: summary,
      areas: [{
        id: "area-1",
        area_key: "frontend",
        composition: { languages: ["TypeScript"] },
        path_scopes: ["ui/"],
        frozen_at: "2026-07-29T12:00:01Z",
        status: "completed",
        attempts: [{
          id: "attempt-1",
          attempt_number: 1,
          status: "completed",
          payload_digest: "digest",
          created_at: "2026-07-29T12:00:02Z",
          terminal_at: "2026-07-29T12:00:03Z",
          is_current: true,
        }],
        accepted_findings: [{
          id: "finding-1",
          attempt_id: "attempt-1",
          guardrail_key: "auth",
          status: "unsupported",
          severity: "warning",
          confidence: 0.75,
          evidence: { path: "ui/src/api/readiness.ts" },
          created_at: "2026-07-29T12:00:03Z",
        }],
        accepted_outputs: [{
          attempt_id: "attempt-1",
          result: { warnings: ["preserved"] },
          created_at: "2026-07-29T12:00:03Z",
        }],
      }],
      area_scores: [{
        area_id: "area-1",
        score: 0.75,
        applicable_weight: 4,
        covered_weight: 3,
        status: "completed",
        created_at: "2026-07-29T12:00:04Z",
      }],
      project_score: { score: 0.75, band: "ready", created_at: "2026-07-29T12:00:04Z" },
      suggestions: [{
        id: "suggestion-1",
        dedupe_key: "auth-remediation",
        suggestion: { action: "Add an auth check" },
        created_at: "2026-07-29T12:00:04Z",
      }],
      events: [{
        id: "event-1",
        event_kind: "fixture_completed",
        payload: { source: "test" },
        created_at: "2026-07-29T12:00:04Z",
      }],
    };
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(detail));
    vi.stubGlobal("fetch", fetchMock);

    await expect(fetchReadinessRunDetail("project/one", "run / one")).resolves.toEqual(detail);

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock.mock.calls[0][0]).toBe(
      "https://djinn.example.test/api/projects/project%2Fone/readiness/run%20%2F%20one",
    );
    expect(fetchMock.mock.calls[0][1]).toMatchObject({ credentials: "include" });
  });

  it("sends supplied kickoff keys and decodes new and already-running responses", async () => {
    const created = { run: { ...summary, status: "identifying" }, identification_task_id: "task-1", created: true, reused: false };
    const alreadyRunning = { run: { ...summary, status: "analyzing" }, identification_task_id: "task-1", created: false, reused: true };
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(jsonResponse(created))
      .mockResolvedValueOnce(jsonResponse(alreadyRunning));
    vi.stubGlobal("fetch", fetchMock);

    await expect(kickoffReadiness("project / one", { idempotencyKey: "explicit-key" })).resolves.toEqual(created);
    await expect(kickoffReadiness("project / one", { idempotencyKey: "other-key" })).resolves.toEqual(alreadyRunning);

    for (const [index, key] of ["explicit-key", "other-key"].entries()) {
      expect(fetchMock.mock.calls[index][0]).toBe(
        "https://djinn.example.test/api/projects/project%20%2F%20one/readiness/kickoff",
      );
      expect(fetchMock.mock.calls[index][1]).toMatchObject({
        method: "POST",
        credentials: "include",
        headers: { Accept: "application/json", "Content-Type": "application/json" },
      });
      expect(JSON.parse((fetchMock.mock.calls[index][1] as RequestInit).body as string)).toEqual({
        idempotency_key: key,
      });
    }
  });

  it("reuses one action key across an explicit retry", async () => {
    const response = { run: summary, identification_task_id: "task-1", created: false, reused: true };
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(response));
    vi.stubGlobal("fetch", fetchMock);
    const action = createReadinessKickoffAction("one-user-action");

    await action.kickoff("project-1");
    await action.kickoff("project-1");

    expect(action.idempotencyKey).toBe("one-user-action");
    expect(fetchMock).toHaveBeenCalledTimes(2);
    for (const call of fetchMock.mock.calls) {
      expect(JSON.parse((call[1] as RequestInit).body as string)).toEqual({
        idempotency_key: "one-user-action",
      });
    }
  });

  it("preserves unauthorized, missing, conflict, and server HTTP failures", async () => {
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(jsonResponse({ code: "authentication_required" }, 401))
      .mockResolvedValueOnce(jsonResponse({ code: "not_found" }, 404))
      .mockResolvedValueOnce(jsonResponse({ code: "kickoff_unavailable" }, 409))
      .mockResolvedValueOnce(jsonResponse({ code: "persistence_failure" }, 500));
    vi.stubGlobal("fetch", fetchMock);

    const calls = [
      fetchActiveOrLatestReadiness("project-1"),
      fetchReadinessRunDetail("project-1", "run-1"),
      kickoffReadiness("project-1", { idempotencyKey: "key-1" }),
      fetchActiveOrLatestReadiness("project-1"),
    ];
    const expected = [
      [401, "authentication_required", "unauthorized"],
      [404, "not_found", "missing"],
      [409, "kickoff_unavailable", "conflict"],
      [500, "persistence_failure", "server"],
    ] as const;

    for (const [index, call] of calls.entries()) {
      await expect(call).rejects.toMatchObject({
        name: "ReadinessHttpError",
        status: expected[index][0],
        code: expected[index][1],
        kind: expected[index][2],
      } satisfies Partial<ReadinessHttpError>);
    }
  });
});
