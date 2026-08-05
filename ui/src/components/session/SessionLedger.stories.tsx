/**
 * Fixtures are the real task 3j5q ("Recover the covered launch cut…") — its
 * acceptance criteria, its two worker sessions and six reviewer sessions, and
 * the actual shell commands the running reviewer issued. Invented data would
 * have hidden the two findings these stories exist to show: that 24 of the
 * reviewer's tool calls never reach the screen, and that its "still running"
 * state is really attempt #6 after five failures.
 */

import { SessionLedger } from "./SessionLedger";
import type {
  AcceptanceCriterion,
  ActivityStep,
  LedgerAgentStatus,
  LedgerEntry,
} from "./ledger";

const meta = {
  title: "Session/SessionLedger",
  component: SessionLedger,
  parameters: { layout: "fullscreen" },
  decorators: [
    (StoryFn: () => React.ReactElement) => (
      <div className="h-screen">
        <StoryFn />
      </div>
    ),
  ],
};

export default meta;

// ── Real task data ──────────────────────────────────────────────────────────

const TASK = {
  taskShortId: "3j5q",
  taskTitle: "Recover the covered launch cut and prove authoritative frame adaptation",
  usageLabel: "5.7M in · 38k out · 4.5M cache",
};

const BRIEF_BODY =
  "Recover the valuable implementation from superseded source branch task/lzy1 by applying " +
  "the equivalent of four non-merge commits onto current main (resolve by preserving " +
  "current-main behavior). Preserve durable credentials.id threading, the explicit " +
  "covered/uncovered launch split, prepare/dispatch fence → start_sse_attempt_v1 → " +
  "mark_active, pre-active permit cleanup, and removal of the legacy mid-stream reissue. " +
  "Finish only the provider frame-adaptation proof.";

const criteria = (metCount: number): AcceptanceCriterion[] => {
  const all: AcceptanceCriterion[] = [
    {
      text: "Durable credential-row identity, covered/uncovered split, dispatch-fence ordering, pre-active Drop cleanup and mid-stream reissue removal recovered on current main.",
      met: false,
      metAt: "19:41",
    },
    {
      text: "All four sse_frame_parser_v1 implementations delegate to their existing authoritative parser and do not introduce a second format parser.",
      met: false,
      metAt: "20:02",
    },
    {
      text: "Deterministic fixtures drive raw B1 SseFrame values through the provider trait seam for all four formats, asserting nonterminal events plus StreamEvent::Done.",
      met: false,
      metAt: "20:14",
    },
    {
      text: "cargo test -p djinn-provider and -p djinn-slot --lib pass with the repository's supported SQLx test configuration.",
      met: false,
      note: "not run · worker flagged the launcher rejected the invocation",
    },
  ];
  return all.map((c, i) => (i < metCount ? { ...c, met: true } : { ...c, metAt: undefined }));
};

// Verbatim from the running reviewer session's tool_use blocks.
const REVIEW_STEPS: ActivityStep[] = [
  { kind: "thinking", label: "Preparing unified diff inspection", meta: "0.4s" },
  { kind: "tool", label: "shell", detail: "git merge-base origin/main HEAD && git status --short", meta: "1.2s" },
  { kind: "tool", label: "shell", detail: "git diff --unified=80 $BASE..HEAD -- djinn-provider/", meta: "2.1s" },
  { kind: "tool", label: "shell", detail: "git diff --unified=80 $BASE..HEAD -- djinn-slot/reply_loop/", meta: "1.8s" },
  { kind: "tool", label: "shell", detail: "git diff --unified=50 $BASE..HEAD -- djinn-db/repositories/", meta: "1.1s" },
  { kind: "tool", label: "shell", detail: "junk scan · git diff --name-only | grep -E '(target|dist)'", meta: "0.6s" },
  { kind: "tool", label: "shell", detail: "rg -n 'sse_frame_parser_v1|SseFrame|StreamEvent::Done'", meta: "12 hits" },
  { kind: "thinking", label: "Inspecting provider test files", meta: "0.3s" },
  { kind: "tool", label: "shell", detail: "nl -ba djinn-slot/src/reply_loop/turn.rs | sed -n '1,280p'", meta: "0.9s" },
  { kind: "tool", label: "shell", detail: "rg -n 'credential(_id|s\\.id)|ResolvedProvider'", meta: "8 hits" },
  { kind: "tool", label: "shell", detail: "nl -ba djinn-provider/src/provider/mod.rs | sed -n '130,230p'", meta: "0.7s" },
  { kind: "tool", label: "shell", detail: "rg -n 'sse_frame_parser_v1' format/openai.rs", meta: "3 hits" },
  { kind: "tool", label: "shell", detail: "rg -n 'sse_frame_parser_v1' format/openai_responses.rs", meta: "3 hits" },
  { kind: "tool", label: "shell", detail: "rg -n 'sse_frame_parser_v1' format/google.rs", meta: "2 hits" },
  { kind: "thinking", label: "Extracting provider test lines with sed", meta: "0.3s" },
  { kind: "tool", label: "shell", detail: "nl -ba reply_loop/turn.rs | sed -n '900,1040p'", meta: "0.8s" },
  { kind: "tool", label: "shell", detail: "git diff --unified=3 $BASE..HEAD -- djinn-provider/", meta: "1.4s" },
  { kind: "tool", label: "shell", detail: "for f in anthropic/streaming.rs openai.rs google.rs; do …", meta: "2.2s" },
  { kind: "tool", label: "shell", detail: "git diff --unified=5 $BASE..HEAD -- reply_loop/streaming.rs", meta: "1.0s" },
  { kind: "thinking", label: "Locating new tests via git diff search", meta: "0.5s" },
  { kind: "tool", label: "shell", detail: "git diff --check $BASE..HEAD; env | grep DATABASE_URL", meta: "0.4s" },
  { kind: "tool", label: "shell", detail: "cargo test --manifest-path server/Cargo.toml -p djinn-provider", meta: "48s" },
  { kind: "tool", label: "shell", detail: "cargo test --manifest-path server/Cargo.toml -p djinn-slot --lib", meta: "1m12s" },
  { kind: "tool", label: "shell", detail: "rg -n '\\.sse_frame_parser_v1\\(|LlmProvider::sse_frame_parser_v1'", meta: "6 hits" },
  { kind: "tool", label: "task_show", detail: "3j5q", meta: "0.2s" },
  { kind: "tool", label: "task_activity_list", detail: "3j5q · limit 50", meta: "0.3s" },
  { kind: "tool", label: "output_grep", detail: "pattern: 'test result:'", meta: "2 hits" },
  { kind: "tool", label: "shell", detail: "nl -ba djinn-db/repositories/credential.rs | sed -n '100,175p'", meta: "0.6s" },
];

const WORKER_STEPS: ActivityStep[] = [
  { kind: "thinking", label: "Inspecting branch commit diffs", meta: "0.5s" },
  { kind: "tool", label: "shell", detail: "git log --oneline task/lzy1 ^main", meta: "0.8s" },
  { kind: "thinking", label: "Investigating file line count mismatch", meta: "0.4s" },
  { kind: "tool", label: "Read", detail: "djinn-provider/src/provider/mod.rs", meta: "0.1s" },
  { kind: "tool", label: "Grep", detail: "sse_frame_parser_v1", meta: "9 hits" },
  { kind: "thinking", label: "Planning selective patch application", meta: "0.6s" },
  { kind: "tool", label: "Edit", detail: "format/anthropic/streaming.rs", meta: "0.2s" },
  { kind: "tool", label: "Edit", detail: "format/openai.rs", meta: "0.2s" },
  { kind: "tool", label: "Edit", detail: "format/openai_responses.rs", meta: "0.2s" },
  { kind: "tool", label: "Edit", detail: "format/google.rs", meta: "0.2s" },
  { kind: "thinking", label: "Planning deterministic test fixtures", meta: "0.4s" },
  { kind: "tool", label: "Read", detail: "djinn-slot/src/reply_loop/tests.rs", meta: "0.1s" },
  { kind: "tool", label: "Edit", detail: "djinn-slot/src/reply_loop/streaming_retry_tests.rs", meta: "0.3s" },
  { kind: "tool", label: "shell", detail: "cargo fmt", meta: "3.1s" },
  { kind: "tool", label: "shell", detail: "git diff --check", meta: "0.3s" },
  { kind: "thinking", label: "Deciding to submit without tests", meta: "0.7s" },
  { kind: "tool", label: "Grep", detail: "ProviderSseFrameParserV1::parse", meta: "4 hits" },
  { kind: "tool", label: "Read", detail: "djinn-agent/src/actors/slot/adapter.rs", meta: "0.1s" },
];

const FILES = [
  "server/crates/djinn-provider/src/provider/mod.rs",
  "server/crates/djinn-provider/src/provider/format/anthropic/streaming.rs",
  "server/crates/djinn-provider/src/provider/format/openai.rs",
  "server/crates/djinn-provider/src/provider/format/openai_responses.rs",
  "server/crates/djinn-provider/src/provider/format/google.rs",
  "server/crates/djinn-slot/src/reply_loop/turn.rs",
  "server/crates/djinn-slot/src/reply_loop/streaming.rs",
  "server/crates/djinn-slot/src/reply_loop/tests.rs",
  "server/crates/djinn-slot/src/reply_loop/streaming_retry_tests.rs",
  "server/crates/djinn-slot/src/helpers/provider_resolution.rs",
  "server/crates/djinn-slot/src/llm_extraction.rs",
  "server/crates/djinn-db/src/repositories/credential.rs",
  "server/crates/djinn-agent/src/actors/slot/adapter.rs",
  "server/crates/djinn-agent/src/actors/slot/helpers/provider_resolution.rs",
  "server/crates/djinn-agent/src/actors/slot/reply_loop/mod.rs",
  "server/crates/djinn-agent/src/supervisor_impl/stage.rs",
  "server/crates/djinn-agent/tests/compaction_hardening/transport.rs",
];

const WORK_SUMMARY =
  "Recovered the covered B1 reply-loop launch path with durable credential-row IDs, fenced " +
  "prepare → no-retry attempt launch → active ordering, pre-active permit cleanup, explicit " +
  "uncovered legacy streaming, and removal of the in-stream reissue path. Added the provider " +
  "trait frame-parser seam and wired all four implementations to their existing authoritative " +
  "format parsers.";

const brief: LedgerEntry = {
  kind: "phase",
  id: "brief",
  title: "Brief",
  turns: [],
  brief: {
    body: BRIEF_BODY,
    filedBy: "fernando",
    timestamp: "18:28",
    facets: [
      { label: "Design" },
      { label: "Source commits", count: 4 },
      { label: "Epic" },
    ],
  },
};

const dispatched: LedgerEntry = {
  kind: "handoff",
  id: "h0",
  to: "worker",
  label: "Dispatched",
  timestamp: "18:30",
};

const implementation = (opts: { withArtifact: boolean }): LedgerEntry => ({
  kind: "phase",
  id: "p1",
  title: "Implementation",
  agentType: "worker",
  modelId: "openai/gpt-5.6-terra",
  durationLabel: "36m",
  turns: [
    {
      id: "t1",
      agentType: "worker",
      blocks: [
        {
          kind: "say",
          markdown:
            "The tests are internal module fixtures imported only by each provider format's " +
            "`#[cfg(test)] mod tests`; they exercise `LlmProvider::sse_frame_parser_v1` and " +
            "`ProviderSseFrameParserV1::parse` without changing production parser APIs or wire schemas.",
        },
        { kind: "activity", steps: WORKER_STEPS, durationLabel: "6m04s" },
        ...(opts.withArtifact
          ? [
              {
                kind: "artifact" as const,
                variant: "work_submitted" as const,
                summary: WORK_SUMMARY,
                files: FILES,
                concerns: [
                  "The scoped provider test command could not be completed in-session after the task-run launcher rejected the invocation; formatter and diff whitespace checks completed.",
                ],
                timestamp: "19:52",
              },
            ]
          : []),
      ],
    },
  ],
});

const handoffToReview: LedgerEntry = {
  kind: "handoff",
  id: "h1",
  from: "worker",
  to: "reviewer",
  label: "Handoff",
  timestamp: "19:54",
};

const agentsMid: LedgerAgentStatus[] = [
  { agentType: "worker", durationLabel: "36m", status: "submitted" },
  { agentType: "reviewer", durationLabel: "12m", status: "running", running: true },
];

// ── Stories ─────────────────────────────────────────────────────────────────

export const BriefOnly = {
  args: {
    ...TASK,
    statusLabel: "Open",
    criteria: criteria(0),
    agents: [],
    entries: [brief, dispatched],
    live: null,
  },
};

export const ImplementationRunning = {
  args: {
    ...TASK,
    statusLabel: "In Progress",
    criteria: criteria(2),
    agents: [{ agentType: "worker", durationLabel: "22m", status: "running", running: true }],
    entries: [
      brief,
      dispatched,
      {
        ...(implementation({ withArtifact: false }) as { kind: "phase" }),
        running: true,
        durationLabel: "22m",
        turns: [
          {
            id: "t1",
            agentType: "worker",
            blocks: [
              {
                kind: "activity",
                steps: WORKER_STEPS.slice(0, 12),
                durationLabel: "22m",
                running: true,
                nowLabel: "editing djinn-slot/src/reply_loop/streaming_retry_tests.rs",
              },
            ],
          },
        ],
      } as LedgerEntry,
    ],
    live: {
      agentType: "worker",
      durationLabel: "22m",
      stepLabel: "step 12",
      nowLabel: "editing streaming_retry_tests.rs",
    },
  },
};

export const WorkSubmitted = {
  args: {
    ...TASK,
    statusLabel: "In Review",
    criteria: criteria(3),
    agents: [{ agentType: "worker", durationLabel: "36m", status: "submitted" }],
    entries: [brief, dispatched, implementation({ withArtifact: true })],
    live: null,
  },
};

/**
 * The screen this redesign exists for. In production this state is
 * indistinguishable from a hung session: no phase, no handoff, no liveness,
 * and every one of the reviewer's 24 tool calls dropped before render.
 */
export const ReviewRunning = {
  args: {
    ...TASK,
    statusLabel: "In Review",
    criteria: criteria(3),
    agents: agentsMid,
    entries: [
      brief,
      dispatched,
      implementation({ withArtifact: true }),
      handoffToReview,
      {
        kind: "phase",
        id: "p2",
        title: "Review",
        agentType: "reviewer",
        modelId: "openai/gpt-5.6-sol",
        durationLabel: "12m",
        running: true,
        turns: [
          {
            id: "t2",
            agentType: "reviewer",
            blocks: [
              {
                kind: "activity",
                steps: REVIEW_STEPS,
                durationLabel: "12m",
                running: true,
                nowLabel: "inspecting provider test files",
              },
            ],
          },
        ],
      } as LedgerEntry,
    ],
    live: {
      agentType: "reviewer",
      durationLabel: "12m",
      stepLabel: "step 28",
      nowLabel: "inspecting provider test files",
    },
  },
};

export const LeadInterjection = {
  args: {
    ...TASK,
    statusLabel: "In Review",
    criteria: criteria(3),
    agents: [
      ...agentsMid,
      { agentType: "lead", durationLabel: "2m", status: "decided" },
    ],
    entries: [
      brief,
      dispatched,
      implementation({ withArtifact: true }),
      handoffToReview,
      {
        kind: "phase",
        id: "p2",
        title: "Review",
        agentType: "reviewer",
        modelId: "openai/gpt-5.6-sol",
        durationLabel: "39m",
        turns: [
          {
            id: "t2",
            agentType: "reviewer",
            blocks: [
              { kind: "activity", steps: REVIEW_STEPS, durationLabel: "39m" },
              {
                kind: "artifact",
                variant: "review_submitted",
                outcome: "Changes requested",
                summary:
                  "The four frame-parser implementations delegate correctly and the fixtures assert " +
                  "nonterminal text/tool/usage plus terminal Done. Criterion 4 is unproven: the scoped " +
                  "cargo test commands were never executed in the implementation session.",
                timestamp: "20:33",
              },
            ],
          },
        ],
      } as LedgerEntry,
      {
        kind: "handoff",
        id: "h2",
        from: "reviewer",
        to: "lead",
        label: "Escalated",
        timestamp: "20:34",
      } as LedgerEntry,
      {
        kind: "phase",
        id: "p3",
        title: "Lead",
        agentType: "lead",
        modelId: "anthropic/claude-opus-5",
        durationLabel: "2m",
        turns: [
          {
            id: "t3",
            agentType: "lead",
            blocks: [
              {
                kind: "artifact",
                variant: "lead_decision",
                outcome: "Continue",
                summary:
                  "Criterion 4 is environmental, not a defect in the change. Return to the worker to " +
                  "run the scoped test commands; do not re-review the frame-adaptation work.",
                timestamp: "20:36",
              },
            ],
          },
        ],
      } as LedgerEntry,
    ],
    live: null,
  },
};

export const Complete = {
  args: {
    ...TASK,
    statusLabel: "Merged",
    criteria: criteria(4),
    agents: [
      { agentType: "worker", durationLabel: "36m", status: "submitted" },
      { agentType: "reviewer", durationLabel: "39m", status: "approved" },
    ],
    entries: [
      brief,
      dispatched,
      implementation({ withArtifact: true }),
      handoffToReview,
      {
        kind: "phase",
        id: "p2",
        title: "Review",
        agentType: "reviewer",
        modelId: "openai/gpt-5.6-sol",
        durationLabel: "39m",
        turns: [
          {
            id: "t2",
            agentType: "reviewer",
            blocks: [
              { kind: "activity", steps: REVIEW_STEPS, durationLabel: "39m" },
              {
                kind: "artifact",
                variant: "review_submitted",
                outcome: "Approved",
                summary:
                  "All four covered formats delegate to their authoritative parsers, fixtures assert " +
                  "representative nonterminal events plus terminal Done, and both scoped test commands pass.",
                timestamp: "21:14",
              },
            ],
          },
        ],
      } as LedgerEntry,
    ],
    live: null,
  },
};
