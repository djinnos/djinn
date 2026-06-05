/**
 * VerificationEditor — edit a project's verification rules with a
 * Test→Save gate.
 *
 * Rules are loaded via `project_verification_get`. The user edits a list of
 * {match_pattern, commands[]}. A non-empty rule set can only be SAVED once
 * a `project_verification_test` run has PASSED for those exact rules — the
 * passing `test_id` is tracked here and invalidated the moment the rules
 * change, forcing a re-test. An empty rule set may be saved without a test
 * (the backend allows it). Backend errors surface via toast.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  CheckmarkCircle02Icon,
  Cancel01Icon,
  Delete02Icon,
  FloppyDiskIcon,
  Loading02Icon,
  PlayIcon,
  PlusSignIcon,
} from "@hugeicons/core-free-icons";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import {
  getVerificationRules,
  isTerminal,
  pollTestStatus,
  setVerificationRules,
  startVerificationTest,
  type CommandResult,
  type TestStatus,
  type VerificationRule,
} from "@/api/verification";
import { showToast } from "@/lib/toast";

const POLL_INTERVAL_MS = 3000;

/** Stable identity for a rule set, so edits invalidate a prior pass. */
function rulesKey(rules: VerificationRule[]): string {
  return JSON.stringify(
    rules.map((r) => ({ m: r.match_pattern, c: r.commands })),
  );
}

export function VerificationEditor({ projectId }: { projectId: string }) {
  const [rules, setRules] = useState<VerificationRule[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  // Test-gate state. `passedKey` records the rulesKey that passed; if the
  // current rules diverge from it, the pass is stale and Save re-locks.
  const [testing, setTesting] = useState(false);
  const [testStatus, setTestStatus] = useState<TestStatus | null>(null);
  const [passedTestId, setPassedTestId] = useState<string | null>(null);
  const [passedKey, setPassedKey] = useState<string | null>(null);
  const pollTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const currentKey = rulesKey(rules);
  const hasFreshPass = passedTestId !== null && passedKey === currentKey;
  const isEmpty = rules.length === 0;
  // Empty rule sets save without a test; non-empty needs a fresh pass.
  const canSave = isEmpty || hasFreshPass;

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const fetched = await getVerificationRules(projectId);
      setRules(fetched);
    } catch (err) {
      const message = err instanceof Error ? err.message : "Failed to load rules";
      showToast.error("Could not load verification rules", { description: message });
    } finally {
      setLoading(false);
    }
  }, [projectId]);

  useEffect(() => {
    void load();
  }, [load]);

  // Tear down any in-flight poll on unmount.
  useEffect(() => {
    return () => {
      if (pollTimer.current) clearTimeout(pollTimer.current);
    };
  }, []);

  // Any rule mutation invalidates a prior pass and clears stale results.
  const mutateRules = (next: VerificationRule[]) => {
    setRules(next);
    setPassedTestId(null);
    setPassedKey(null);
    setTestStatus(null);
  };

  const addRule = () => {
    mutateRules([...rules, { match_pattern: "", commands: [""] }]);
  };
  const removeRule = (idx: number) => {
    const next = rules.slice();
    next.splice(idx, 1);
    mutateRules(next);
  };
  const setMatch = (idx: number, value: string) => {
    const next = rules.slice();
    next[idx] = { ...next[idx], match_pattern: value };
    mutateRules(next);
  };
  const setCommands = (idx: number, value: string) => {
    const next = rules.slice();
    next[idx] = {
      ...next[idx],
      commands: value.split("\n").map((s) => s.trim()).filter(Boolean),
    };
    mutateRules(next);
  };

  const handleTest = async () => {
    // Snapshot the rules under test so a late edit can't mark a pass fresh.
    const testedKey = currentKey;
    setTesting(true);
    setTestStatus({ run_status: "pending", passed: false, results: [] });
    setPassedTestId(null);
    setPassedKey(null);
    try {
      const started = await startVerificationTest(projectId, rules);
      if (!started.ok || !started.testId) {
        showToast.error("Could not start test", { description: started.error });
        setTesting(false);
        setTestStatus(null);
        return;
      }
      const testId = started.testId;
      const poll = async () => {
        try {
          const status = await pollTestStatus(testId);
          setTestStatus(status);
          if (isTerminal(status.run_status)) {
            setTesting(false);
            if (status.run_status === "passed") {
              setPassedTestId(testId);
              setPassedKey(testedKey);
              showToast.success("Verification passed", {
                description: "You can now save these rules.",
              });
            } else if (status.run_status === "failed") {
              showToast.error("Verification failed", {
                description: "Fix the failing commands and re-test.",
              });
            } else {
              showToast.error("Test run errored", {
                description: status.run_error ?? undefined,
              });
            }
            return;
          }
          pollTimer.current = setTimeout(() => void poll(), POLL_INTERVAL_MS);
        } catch (err) {
          setTesting(false);
          const message = err instanceof Error ? err.message : "Polling failed";
          showToast.error("Could not read test status", { description: message });
        }
      };
      pollTimer.current = setTimeout(() => void poll(), POLL_INTERVAL_MS);
    } catch (err) {
      setTesting(false);
      setTestStatus(null);
      const message = err instanceof Error ? err.message : "Failed to start test";
      showToast.error("Could not start test", { description: message });
    }
  };

  const handleSave = async () => {
    setSaving(true);
    try {
      const result = await setVerificationRules(
        projectId,
        rules,
        isEmpty ? undefined : passedTestId ?? undefined,
      );
      if (!result.ok) {
        showToast.error("Save failed", { description: result.error });
        return;
      }
      showToast.success("Verification rules saved");
      await load();
      // A successful save means the persisted rules == current; keep the
      // pass valid so the user can re-save without re-testing.
    } catch (err) {
      const message = err instanceof Error ? err.message : "Save failed";
      showToast.error("Save failed", { description: message });
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return (
      <div className="flex h-40 items-center justify-center">
        <HugeiconsIcon icon={Loading02Icon} className="size-5 animate-spin text-muted-foreground" />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-5">
      <div className="flex items-start justify-between gap-3">
        <p className="text-xs text-muted-foreground">
          Commands that prove a task-run succeeded. Rules match on changed files via glob. A
          non-empty rule set must pass a test run before it can be saved.
        </p>
        <div className="flex shrink-0 items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            className="h-8 gap-1.5 text-xs"
            onClick={() => void handleTest()}
            disabled={testing || isEmpty || saving}
          >
            {testing ? (
              <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
            ) : (
              <HugeiconsIcon icon={PlayIcon} size={14} />
            )}
            {testing ? "Testing…" : "Test"}
          </Button>
          <Button
            size="sm"
            className="h-8 gap-1.5 text-xs"
            onClick={() => void handleSave()}
            disabled={saving || testing || !canSave}
            title={
              canSave
                ? undefined
                : "Run a passing test before saving"
            }
          >
            {saving ? (
              <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
            ) : (
              <HugeiconsIcon icon={FloppyDiskIcon} size={14} />
            )}
            {saving ? "Saving…" : "Save"}
          </Button>
        </div>
      </div>

      {!isEmpty && !hasFreshPass && !testing && (
        <p className="text-xs text-amber-300">
          {passedTestId === null
            ? "These rules haven't passed a test yet — run Test to unlock Save."
            : "Rules changed since the last passing test — re-run Test to unlock Save."}
        </p>
      )}

      <TestResultPanel testing={testing} status={testStatus} />

      <div className="flex flex-col gap-3">
        {rules.length === 0 && (
          <p className="text-xs text-muted-foreground">
            No verification rules. An empty set can be saved directly.
          </p>
        )}
        {rules.map((rule, idx) => (
          <div key={idx} className="rounded-md border bg-background/30 p-3">
            <div className="flex items-center justify-between gap-2 pb-2">
              <div className="flex flex-1 flex-col gap-1">
                <Label className="text-xs text-muted-foreground">Match pattern (glob)</Label>
                <Input
                  value={rule.match_pattern}
                  onChange={(e) => setMatch(idx, e.target.value)}
                  placeholder="src/**/*.rs"
                  className="font-mono text-xs"
                />
              </div>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                className="h-7 gap-1 self-end px-2 text-muted-foreground hover:text-red-400"
                onClick={() => removeRule(idx)}
              >
                <HugeiconsIcon icon={Delete02Icon} size={14} />
              </Button>
            </div>
            <div>
              <Label className="text-xs text-muted-foreground">Commands (one per line)</Label>
              <Textarea
                value={rule.commands.join("\n")}
                onChange={(e) => setCommands(idx, e.target.value)}
                placeholder="cargo test"
                className="mt-1 min-h-[60px] font-mono text-xs"
              />
            </div>
          </div>
        ))}
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="w-fit gap-1.5 text-xs"
          onClick={addRule}
        >
          <HugeiconsIcon icon={PlusSignIcon} size={12} />
          Add rule
        </Button>
      </div>
    </div>
  );
}

// ── Test results ──────────────────────────────────────────────────────────

function TestResultPanel({
  testing,
  status,
}: {
  testing: boolean;
  status: TestStatus | null;
}) {
  if (!status) return null;

  const running = testing || status.run_status === "pending" || status.run_status === "running";

  return (
    <div className="rounded-md border bg-background/30 p-3">
      <div className="flex items-center gap-2 pb-2">
        {running ? (
          <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin text-sky-300" />
        ) : status.run_status === "passed" ? (
          <HugeiconsIcon icon={CheckmarkCircle02Icon} size={14} className="text-emerald-300" />
        ) : (
          <HugeiconsIcon icon={Cancel01Icon} size={14} className="text-red-300" />
        )}
        <span className="text-sm font-medium">
          {running
            ? status.run_status === "pending"
              ? "Test pending…"
              : "Test running…"
            : status.run_status === "passed"
            ? "Passed"
            : status.run_status === "failed"
            ? "Failed"
            : "Errored"}
        </span>
      </div>

      {status.run_error && (
        <p className="pb-2 text-xs text-red-300">{status.run_error}</p>
      )}

      {status.results.length > 0 && (
        <div className="flex flex-col gap-2">
          {status.results.map((result, idx) => (
            <CommandResultRow key={idx} result={result} />
          ))}
        </div>
      )}
    </div>
  );
}

function CommandResultRow({ result }: { result: CommandResult }) {
  const ok = result.exit_code === 0;
  const label = result.command ?? result.name ?? "command";
  return (
    <div className="rounded border border-border/40 bg-background/40 p-2">
      <div className="flex items-center justify-between gap-2">
        <code className="truncate font-mono text-xs text-foreground">{label}</code>
        <span
          className={
            ok
              ? "shrink-0 text-xs text-emerald-300"
              : "shrink-0 text-xs text-red-300"
          }
        >
          exit {result.exit_code ?? "?"}
        </span>
      </div>
      {!ok && (result.stderr || result.stdout) && (
        <pre className="mt-1.5 max-h-40 overflow-auto whitespace-pre-wrap rounded bg-black/30 p-2 font-mono text-[11px] text-muted-foreground">
          {(result.stderr || result.stdout || "").slice(0, 4000)}
        </pre>
      )}
    </div>
  );
}
