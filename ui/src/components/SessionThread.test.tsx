import { describe, expect, it } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import { SessionThread } from '@/components/SessionThread';
import { mapBudgetParkActivity, mapLoopGuardActivity } from '@/hooks/useSessionMessages';
import type { SessionListOutput, SessionShowOutput } from '@/api/generated/mcp-tools.gen';
import type { TimelineEntry } from '@/hooks/useSessionMessages';

const generatedSessionShowBindingKeepsParkedReason: SessionShowOutput = {
  id: 'session-budget',
  status: 'completed',
  parked_reason: 'budget',
};

const generatedSessionListBindingKeepsParkedReason: SessionListOutput = {
  sessions: [{
    id: 'session-budget',
    agent_type: 'worker',
    model_id: 'claude-test',
    started_at: '2026-01-01T00:00:00Z',
    status: 'completed',
    tokens_in: 1,
    tokens_out: 2,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    parked_reason: 'budget',
  }],
};

function makeMessage(overrides: Partial<Extract<TimelineEntry, { kind: 'message' }>> = {}): Extract<TimelineEntry, { kind: 'message' }> {
  return {
    kind: 'message',
    role: 'assistant',
    agentType: 'worker',
    content: [{ type: 'text', text: 'Assistant response' }],
    timestamp: '2026-01-01T00:00:00Z',
    ...overrides,
  };
}

describe('SessionThread', () => {
  it('keeps parked_reason in generated session list/show bindings', () => {
    expect(generatedSessionShowBindingKeepsParkedReason.parked_reason).toBe('budget');
    expect(generatedSessionListBindingKeepsParkedReason.sessions?.[0]?.parked_reason).toBe('budget');
  });

  it('renders assistant messages and hides user messages', () => {
    const timeline: TimelineEntry[] = [
      makeMessage({
        role: 'user',
        agentType: 'worker',
        content: [{ type: 'text', text: 'User prompt' }],
      }),
      makeMessage({
        role: 'assistant',
        agentType: 'worker',
        content: [{ type: 'text', text: 'Assistant response' }],
      }),
    ];

    render(
      <SessionThread
        timeline={timeline}
        streamingText={new Map()}
        loading={false}
        error={null}
      />
    );

    // User messages are hidden in the session thread
    expect(screen.queryByText('User prompt')).not.toBeInTheDocument();
    expect(screen.getByText('Assistant response')).toBeInTheDocument();
    expect(screen.getByText('Worker')).toBeInTheDocument();
  });

  it('shows streaming indicator during active streaming', () => {
    render(
      <SessionThread
        timeline={[]}
        streamingText={new Map([['session-1', 'Streaming now']])}
        loading={false}
        error={null}
        activeAgentType="worker"
      />
    );

    expect(screen.getByText('Streaming now')).toBeInTheDocument();
    const streamingParagraph = screen.getByText('Streaming now');
    const streamingBubble = streamingParagraph.parentElement?.parentElement;
    expect(streamingBubble).not.toBeNull();
    expect(within(streamingBubble as HTMLElement).getByText('Worker')).toBeInTheDocument();
  });

  it('shows empty state for session with no activity', () => {
    render(
      <SessionThread
        timeline={[]}
        streamingText={new Map()}
        loading={false}
        error={null}
      />
    );

    expect(screen.getByText('Not dispatched yet')).toBeInTheDocument();
  });

  it('renders loop guard activity as a distinct timeline card', () => {
    const loopGuardEntry = mapLoopGuardActivity({
      kind: 'loop_guard_tripped',
      event_type: 'loop_guard_tripped',
      timestamp: '2026-01-01T00:03:00Z',
      details: {
        kind: 'identical_tool_failure',
        offending_signature: 'shell:cargo-test',
        threshold: 3,
        observed: 4,
        turn_span: { start: 7, end: 12 },
        session_id: 'session-123',
      },
    });

    expect(loopGuardEntry).toMatchObject({
      kind: 'loop_guard_tripped',
      guardKind: 'identical_tool_failure',
      offendingSignature: 'shell:cargo-test',
      threshold: 3,
      observed: 4,
      turnSpan: { start: 7, end: 12 },
      sessionId: 'session-123',
    });
    expect(mapLoopGuardActivity({
      event_type: 'loop_guard_tripped',
      timestamp: '2026-01-01T00:03:00Z',
      payload: { details: { kind: 'identical_output', turn_span: [1, 2] } },
    })).toMatchObject({ guardKind: 'identical_output', turnSpan: { start: 1, end: 2 } });

    if (!loopGuardEntry) throw new Error('expected loop guard timeline entry');

    render(
      <SessionThread
        timeline={[loopGuardEntry]}
        streamingText={new Map()}
        loading={false}
        error={null}
      />
    );

    expect(screen.getByText('Loop guard tripped')).toBeInTheDocument();
    expect(screen.getByText('identical_tool_failure')).toBeInTheDocument();
    expect(screen.getByText('shell:cargo-test')).toBeInTheDocument();
    expect(screen.getByText('7–12')).toBeInTheDocument();
    expect(screen.getByText('3 / 4')).toBeInTheDocument();
    expect(screen.getByText('session-123')).toBeInTheDocument();
    expect(screen.queryByText('Failed')).not.toBeInTheDocument();
    expect(screen.queryByText('Provider failure')).not.toBeInTheDocument();
    expect(screen.queryByText('Budget park')).not.toBeInTheDocument();
    expect(screen.queryByText('Passed')).not.toBeInTheDocument();
  });

  it('renders final tool call as a formatted card', () => {
    const timeline: TimelineEntry[] = [
      makeMessage({
        content: [{ type: 'tool_use', name: 'submit_work', input: { summary: 'Implemented feature X' } }],
      }),
    ];

    render(
      <SessionThread
        timeline={timeline}
        streamingText={new Map()}
        loading={false}
        error={null}
      />
    );

    expect(screen.getByText('Work Submitted')).toBeInTheDocument();
    expect(screen.getByText('Implemented feature X')).toBeInTheDocument();
  });

  it('renders budget-park work_submitted activity as a distinct card', () => {
    const budgetParkEntry = mapBudgetParkActivity(
      {
        event_type: 'work_submitted',
        timestamp: '2026-01-01T00:04:00Z',
        payload: {
          session_id: 'session-budget',
          summary: 'Implemented the safe subset before budget ran out.',
          remaining_concerns: 'budget-parked: finish the UI snapshot update',
        },
      },
      new Map([['session-budget', { id: 'session-budget', parked_reason: 'budget' }]])
    );

    expect(budgetParkEntry).toMatchObject({
      kind: 'budget_park',
      summary: 'Implemented the safe subset before budget ran out.',
      remainingConcerns: 'budget-parked: finish the UI snapshot update',
      parkedReason: 'budget',
      sessionId: 'session-budget',
    });
    expect(mapBudgetParkActivity(
      {
        event_type: 'work_submitted',
        timestamp: '2026-01-01T00:04:00Z',
        payload: {
          summary: 'ordinary submit',
          remaining_concerns: 'budget-parked: but no parked session',
        },
      },
      new Map()
    )).toBeNull();

    expect(mapBudgetParkActivity(
      {
        event_type: 'work_submitted',
        timestamp: '2026-01-01T00:04:00Z',
        payload: {
          session_id: 'session-completed',
          summary: 'ordinary submit',
          remaining_concerns: 'none',
        },
      },
      new Map([['session-completed', { id: 'session-completed', status: 'completed' }]])
    )).toBeNull();
    expect(mapBudgetParkActivity(
      {
        event_type: 'work_submitted',
        timestamp: '2026-01-01T00:04:00Z',
        payload: {
          session_id: 'session-paused',
          summary: 'pause handoff',
          remaining_concerns: 'budget-parked: should not map without budget reason',
        },
      },
      new Map([['session-paused', { id: 'session-paused', status: 'paused', parked_reason: 'operator_pause' }]])
    )).toBeNull();
    expect(mapBudgetParkActivity(
      {
        event_type: 'failed',
        kind: 'provider_failure',
        timestamp: '2026-01-01T00:04:00Z',
        payload: {
          session_id: 'session-budget',
          remaining_concerns: 'budget-parked: provider failures are not work_submitted handoffs',
        },
      },
      new Map([['session-budget', { id: 'session-budget', parked_reason: 'budget' }]])
    )).toBeNull();
    expect(mapBudgetParkActivity(
      {
        event_type: 'loop_guard_tripped',
        kind: 'loop_guard_tripped',
        timestamp: '2026-01-01T00:04:00Z',
        payload: {
          session_id: 'session-budget',
          remaining_concerns: 'budget-parked: loop guards are rendered separately',
        },
      },
      new Map([['session-budget', { id: 'session-budget', parked_reason: 'budget' }]])
    )).toBeNull();

    if (!budgetParkEntry) throw new Error('expected budget park timeline entry');

    render(
      <SessionThread
        timeline={[budgetParkEntry]}
        streamingText={new Map()}
        loading={false}
        error={null}
      />
    );

    expect(screen.getByText('Budget park')).toBeInTheDocument();
    expect(screen.getByText('Budget park — summary')).toBeInTheDocument();
    expect(screen.getByText('parked_reason: budget')).toBeInTheDocument();
    expect(screen.getByText('Implemented the safe subset before budget ran out.')).toBeInTheDocument();
    expect(screen.getByText('budget-parked: finish the UI snapshot update')).toBeInTheDocument();
    expect(screen.queryByText('Work Submitted')).not.toBeInTheDocument();
    expect(screen.queryByText('Loop guard tripped')).not.toBeInTheDocument();
    expect(screen.queryByText('Provider failure')).not.toBeInTheDocument();
  });

  it('shows loading state when loading and no timeline yet', () => {
    render(
      <SessionThread
        timeline={[]}
        streamingText={new Map()}
        loading={true}
        error={null}
      />
    );

    expect(screen.getByText('Loading session history…')).toBeInTheDocument();
  });
});
