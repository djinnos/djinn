import { describe, expect, it } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import { SessionThread } from '@/components/SessionThread';
import { mapBudgetParkActivity, mapLoopGuardActivity } from '@/hooks/useSessionMessages';
import type { TimelineEntry } from '@/hooks/useSessionMessages';

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
