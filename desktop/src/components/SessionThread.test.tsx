import { describe, expect, it } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import { SessionThread } from '@/components/SessionThread';
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
