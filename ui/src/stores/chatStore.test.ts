import { beforeEach, describe, expect, it } from 'vitest';
import { useChatStore, type ChatMessage, type ChatSession } from './chatStore';

function makeSession(id: string, overrides?: Partial<ChatSession>): ChatSession {
  return {
    id,
    title: 'New Chat',
    projectSlug: null,
    model: null,
    createdAt: 1,
    updatedAt: 1,
    ...overrides,
  };
}

describe('chatStore', () => {
  beforeEach(() => {
    useChatStore.setState({
      sessions: [],
      messagesBySession: {},
      streamingBySession: {},
      loadingBySession: {},
      thinkingStartTimeBySession: {},
      draftBySession: {},
      globalDraft: '',
      activeSessionId: null,
    });
  });

  it('setSessions replaces the in-memory session list', () => {
    useChatStore.getState().setSessions([makeSession('a'), makeSession('b')]);
    expect(useChatStore.getState().sessions.map((s) => s.id)).toEqual(['a', 'b']);
  });

  it('upsertSession inserts a new session and updates existing ones', () => {
    useChatStore.getState().upsertSession(makeSession('a', { title: 'first' }));
    useChatStore.getState().upsertSession(makeSession('b', { title: 'second' }));
    expect(useChatStore.getState().sessions.map((s) => s.id)).toEqual(['b', 'a']);

    useChatStore.getState().upsertSession(makeSession('a', { title: 'updated' }));
    const updated = useChatStore.getState().sessions.find((s) => s.id === 'a')!;
    expect(updated.title).toBe('updated');
    expect(useChatStore.getState().sessions).toHaveLength(2);
  });

  it('removeSession drops a session and its caches, falling back to next active', () => {
    useChatStore.getState().setSessions([makeSession('a'), makeSession('b')]);
    useChatStore.getState().setActiveSession('a');
    useChatStore.getState().setSessionMessages('a', []);
    useChatStore.getState().appendStreamingText('a', 'x');

    useChatStore.getState().removeSession('a');
    const state = useChatStore.getState();
    expect(state.sessions.find((s) => s.id === 'a')).toBeUndefined();
    expect(state.messagesBySession.a).toBeUndefined();
    expect(state.streamingBySession.a).toBeUndefined();
    expect(state.loadingBySession.a).toBeUndefined();
    expect(state.thinkingStartTimeBySession.a).toBeUndefined();
    expect(state.activeSessionId).toBe('b');
  });

  it('setSessionMessages seeds the in-memory cache for a session', () => {
    const msgs: ChatMessage[] = [
      { id: 'm1', role: 'user', content: 'hi', createdAt: 1 },
      { id: 'm2', role: 'assistant', content: 'hello', createdAt: 2 },
    ];
    useChatStore.getState().setSessionMessages('a', msgs);
    expect(useChatStore.getState().messagesBySession.a).toEqual(msgs);
  });

  it('addMessage appends but does not auto-title', () => {
    useChatStore.getState().upsertSession(makeSession('a', { title: 'New Chat' }));
    useChatStore.getState().addMessage('a', {
      id: 'm1',
      role: 'user',
      content: 'This would have auto-titled before',
      createdAt: 1,
    });
    const session = useChatStore.getState().sessions.find((s) => s.id === 'a')!;
    expect(useChatStore.getState().messagesBySession.a).toHaveLength(1);
    // Server owns titling now — the store shouldn't rewrite the title locally.
    expect(session.title).toBe('New Chat');
  });

  it('appendStreamingText appends and sets loading', () => {
    useChatStore.getState().upsertSession(makeSession('a'));
    useChatStore.getState().setThinkingStartTime('a', 123);
    useChatStore.getState().appendStreamingText('a', 'hel');
    useChatStore.getState().appendStreamingText('a', 'lo');
    const state = useChatStore.getState();
    expect(state.streamingBySession.a).toBe('hello');
    expect(state.loadingBySession.a).toBe(true);
    expect(state.thinkingStartTimeBySession.a).toBeNull();
  });

  it('finalizeStreaming creates assistant message from buffer and clears flags', () => {
    useChatStore.getState().upsertSession(makeSession('a'));
    useChatStore.getState().setThinkingStartTime('a', 123);
    useChatStore.getState().appendStreamingText('a', 'stream');
    useChatStore.getState().finalizeStreaming('a');
    const state = useChatStore.getState();
    expect(state.messagesBySession.a).toHaveLength(1);
    expect(state.messagesBySession.a[0].role).toBe('assistant');
    expect(state.messagesBySession.a[0].content).toBe('stream');
    expect(state.streamingBySession.a).toBe('');
    expect(state.loadingBySession.a).toBe(false);
    expect(state.thinkingStartTimeBySession.a).toBeNull();
  });

  it('finalizeStreaming with explicit blank content does not add a message', () => {
    useChatStore.getState().upsertSession(makeSession('a'));
    useChatStore.getState().finalizeStreaming('a', { id: 'x', role: 'assistant', content: '   ', createdAt: 2 });
    expect(useChatStore.getState().messagesBySession.a ?? []).toHaveLength(0);
  });

  it('updateSessionTitle writes a normalized title', () => {
    useChatStore.getState().upsertSession(makeSession('a'));
    useChatStore.getState().updateSessionTitle('a', '  Generated   Title  ');
    expect(useChatStore.getState().sessions.find((s) => s.id === 'a')?.title).toBe('Generated Title');
  });

  it('clearStreaming resets streaming and loading', () => {
    useChatStore.getState().upsertSession(makeSession('a'));
    useChatStore.getState().setThinkingStartTime('a', 123);
    useChatStore.getState().appendStreamingText('a', 'x');
    useChatStore.getState().clearStreaming('a');
    expect(useChatStore.getState().streamingBySession.a).toBe('');
    expect(useChatStore.getState().loadingBySession.a).toBe(false);
    expect(useChatStore.getState().thinkingStartTimeBySession.a).toBeNull();
  });

  it('sets and clears thinking start time per session', () => {
    useChatStore.getState().upsertSession(makeSession('a'));
    useChatStore.getState().setThinkingStartTime('a', 999);
    expect(useChatStore.getState().thinkingStartTimeBySession.a).toBe(999);

    useChatStore.getState().setThinkingStartTime('a', null);
    expect(useChatStore.getState().thinkingStartTimeBySession.a).toBeNull();
  });

  it('setDraft writes per-session and global drafts', () => {
    useChatStore.getState().setDraft('a', 'hello');
    useChatStore.getState().setDraft(null, 'global');
    expect(useChatStore.getState().draftBySession.a).toBe('hello');
    expect(useChatStore.getState().globalDraft).toBe('global');
  });
});
