/**
 * Project verification MCP tool wrappers.
 *
 * Verification rules ({match_pattern, commands[]}) prove a task-run
 * succeeded. The Test→Save gate: a non-empty rule set can only be saved
 * once a `project_verification_test` run has PASSED for those exact rules
 * (the server rejects an untested save so a broken rule can't disrupt
 * live tasks). The four underlying MCP tools (`project_verification_get`,
 * `project_verification_test`, `project_verification_test_status`,
 * `project_verification_set`) are type-checked against the generated
 * `mcp-tools.gen.ts`.
 */
import { callMcpTool } from "@/api/mcpClient";

export interface VerificationRule {
  match_pattern: string;
  commands: string[];
}

/** Lifecycle of a one-shot test run. */
export type TestRunStatus = "pending" | "running" | "passed" | "failed" | "error";

/** Per-command result parsed out of `results_json`. */
export interface CommandResult {
  name?: string;
  command?: string;
  exit_code?: number;
  stdout?: string;
  stderr?: string;
  duration_ms?: number;
}

export interface TestStatus {
  run_status: TestRunStatus;
  passed: boolean;
  results: CommandResult[];
  run_error?: string;
}

export interface MutationResult {
  ok: boolean;
  error?: string;
}

/** Fetch the project's persisted verification rules. */
export async function getVerificationRules(
  project: string,
): Promise<VerificationRule[]> {
  const response = await callMcpTool("project_verification_get", { project });
  if (response.status !== "ok") {
    throw new Error(response.error ?? "Failed to load verification rules");
  }
  return (response.rules ?? []).map((r) => ({
    match_pattern: r.match_pattern,
    commands: r.commands ?? [],
  }));
}

/**
 * Start a one-shot test run of candidate rules. Returns the `test_id` to
 * poll with `pollTestStatus`, then hand to `setVerificationRules` once it
 * has passed.
 */
export async function startVerificationTest(
  project: string,
  rules: VerificationRule[],
): Promise<{ ok: boolean; error?: string; testId?: string }> {
  const response = await callMcpTool("project_verification_test", {
    project,
    rules,
  });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "test failed to start" };
  }
  return { ok: true, testId: response.test_id };
}

/** Poll a test run's current status. */
export async function pollTestStatus(testId: string): Promise<TestStatus> {
  const response = await callMcpTool("project_verification_test_status", {
    test_id: testId,
  });
  if (response.status !== "ok") {
    throw new Error(response.error ?? "Failed to fetch test status");
  }
  return {
    run_status: (response.run_status as TestRunStatus) ?? "pending",
    passed: Boolean(response.passed),
    results: parseResults(response.results_json),
    run_error: response.run_error,
  };
}

/**
 * Persist a verification rule set. A non-empty `rules` set REQUIRES a
 * `testId` from a prior passing test; an empty set may be saved without
 * one.
 */
export async function setVerificationRules(
  project: string,
  rules: VerificationRule[],
  testId?: string,
): Promise<MutationResult> {
  const response = await callMcpTool("project_verification_set", {
    project,
    rules,
    test_id: testId,
  });
  if (response.status !== "ok") {
    return { ok: false, error: response.error ?? "save failed" };
  }
  return { ok: true };
}

/** True once a run reaches a terminal state. */
export function isTerminal(status: TestRunStatus): boolean {
  return status === "passed" || status === "failed" || status === "error";
}

function parseResults(raw: string | undefined | null): CommandResult[] {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed as CommandResult[]) : [];
  } catch {
    return [];
  }
}
