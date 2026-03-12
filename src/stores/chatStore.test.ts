import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useChatStore, type ChatMessage } from './chatStore';

describe('chatStore', () => {
  beforeEach(() => {
    useChatStore.setState({
      sessions: [],
      messagesBySession: {},
      streamingBySession: {},
      loadingBySession: {},
      activeSessionId: null,
    });
  });

  it('creates session and sets it active', () => {
    const id = useChatStore.getState().createSession('/p');
    const state = useChatStore.getState();
    expect(state.activeSessionId).toBe(id);
    expect(state.sessions[0].id).toBe(id);
    expect(state.sessions[0].projectPath).toBe('/p');
    expect(state.messagesBySession[id]).toEqual([]);
  });

  it('deletes session and clears related maps', () => {
    const a = useChatStore.getState().createSession('/p');
    const b = useChatStore.getState().createSession('/p');
    useChatStore.getState().setActiveSession(a);
    useChatStore.getState().deleteSession(a);
    const state = useChatStore.getState();
    expect(state.sessions.find((s) => s.id === a)).toBeUndefined();
    expect(state.messagesBySession[a]).toBeUndefined();
    expect(state.streamingBySession[a]).toBeUndefined();
    expect(state.loadingBySession[a]).toBeUndefined();
    expect(state.activeSessionId).toBe(b);
  });

  it('adds messages and auto-titles first user message', () => {
    const id = useChatStore.getState().createSession(null);
    const msg: ChatMessage = { id: 'm1', role: 'user', content: 'Hello there title', createdAt: 1 };
    useChatStore.getState().addMessage(id, msg);
    const s = useChatStore.getState().sessions.find((x) => x.id === id)!;
    expect(useChatStore.getState().messagesBySession[id]).toHaveLength(1);
    expect(s.title).toBe('Hello there title');
  });

  it('appendStreamingText appends and sets loading', () => {
    const id = useChatStore.getState().createSession(null);
    useChatStore.getState().appendStreamingText(id, 'hel');
    useChatStore.getState().appendStreamingText(id, 'lo');
    const state = useChatStore.getState();
    expect(state.streamingBySession[id]).toBe('hello');
    expect(state.loadingBySession[id]).toBe(true);
  });

  it('finalizeStreaming creates assistant message from buffer and clears flags', () => {
    const id = useChatStore.getState().createSession(null);
    useChatStore.getState().appendStreamingText(id, 'stream');
    useChatStore.getState().finalizeStreaming(id);
    const state = useChatStore.getState();
    expect(state.messagesBySession[id]).toHaveLength(1);
    expect(state.messagesBySession[id][0].role).toBe('assistant');
    expect(state.messagesBySession[id][0].content).toBe('stream');
    expect(state.streamingBySession[id]).toBe('');
    expect(state.loadingBySession[id]).toBe(false);
  });

  it('finalizeStreaming with explicit message does not add blank content', () => {
    const id = useChatStore.getState().createSession(null);
    useChatStore.getState().finalizeStreaming(id, { id: 'x', role: 'assistant', content: '   ', createdAt: 2 });
    expect(useChatStore.getState().messagesBySession[id]).toHaveLength(0);
  });



  it('updates session title directly', () => {
    const id = useChatStore.getState().createSession(null);
    useChatStore.getState().updateSessionTitle(id, 'Generated Title');
    expect(useChatStore.getState().sessions.find((s) => s.id === id)?.title).toBe('Generated Title');
  });

  it('clearStreaming resets streaming and loading', () => {
    const id = useChatStore.getState().createSession(null);
    useChatStore.getState().appendStreamingText(id, 'x');
    useChatStore.getState().clearStreaming(id);
    expect(useChatStore.getState().streamingBySession[id]).toBe('');
    expect(useChatStore.getState().loadingBySession[id]).toBe(false);
  });

  it('filters sessions by project path', () => {
    useChatStore.getState().createSession('/a');
    useChatStore.getState().createSession('/b');
    useChatStore.getState().createSession('/a');
    expect(useChatStore.getState().getSessionsForProject('/a')).toHaveLength(2);
    expect(useChatStore.getState().getSessionsForProject('/b')).toHaveLength(1);
  });

  it('truncates long auto title', () => {
    const id = useChatStore.getState().createSession(null);
    const now = vi.spyOn(Date, 'now').mockReturnValue(123);
    useChatStore.getState().addMessage(id, {
      id: 'u',
      role: 'user',
      content: 'a'.repeat(80),
      createdAt: 1,
    });
    expect(useChatStore.getState().sessions.find((s) => s.id === id)?.title.endsWith('…')).toBe(true);
    now.mockRestore();
  });
});
