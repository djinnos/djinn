import { useEffect, useMemo, useRef, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { fetchProviderModels, type ProviderModel } from '@/api/settings';
import { userSettingsQueryOptions } from '@/api/queryOptions';
import { sendChatMessage } from '@/api/chat';
import { getChatSessionMessages } from '@/api/chatSessions';
import { Shimmer } from '@/components/ai-elements/shimmer';
import { toast } from '@/lib/toast';
import { useChatStore, type ChatAttachment, type ChatMessage } from '@/stores/chatStore';
import { useCodeGraphStore } from '@/stores/codeGraphStore';
import { useIsAllProjects, useSelectedProject } from '@/stores/useProjectStore';
import { useChatToolCallHarvest } from '@/hooks/useChatToolCallHarvest';
import { ChatMessageBubble } from './ChatMessageBubble';
import { ChatInput } from './ChatInput';
import { ChatEmptyState } from './ChatEmptyState';
import { ProposalChatContext } from './ProposalChatContext';
import { AnimatePresence, motion } from 'framer-motion';

const EMPTY_MESSAGES: ChatMessage[] = [];
const MODEL_STORAGE_KEY = 'djinnos-chat-model';

function generateSessionId(): string {
  if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}

export function ChatView() {
  const queryClient = useQueryClient();
  const isAllProjects = useIsAllProjects();
  const selectedProject = useSelectedProject();
  const projectSlug =
    isAllProjects || !selectedProject
      ? null
      : `${selectedProject.github_owner}/${selectedProject.github_repo}`;

  const activeSessionId = useChatStore((state) => state.activeSessionId);
  const activeSession = useChatStore((state) =>
    state.activeSessionId ? state.sessions.find((session) => session.id === state.activeSessionId) ?? null : null
  );
  const setActiveSession = useChatStore((state) => state.setActiveSession);
  const upsertSession = useChatStore((state) => state.upsertSession);
  const setSessionMessages = useChatStore((state) => state.setSessionMessages);
  const setSessionModel = useChatStore((state) => state.setSessionModel);
  const addMessage = useChatStore((state) => state.addMessage);
  const appendStreamingText = useChatStore((state) => state.appendStreamingText);
  const finalizeStreaming = useChatStore((state) => state.finalizeStreaming);
  const updateSessionTitle = useChatStore((state) => state.updateSessionTitle);
  const clearStreaming = useChatStore((state) => state.clearStreaming);
  const setThinkingStartTime = useChatStore((state) => state.setThinkingStartTime);
  const setDraft = useChatStore((state) => state.setDraft);
  const activeScope = useChatStore((state) =>
    state.activeSessionId ? state.scopeBySession[state.activeSessionId] ?? null : null
  );
  const draft = useChatStore((state) =>
    state.activeSessionId ? state.draftBySession[state.activeSessionId] ?? '' : state.globalDraft
  );
  const messages = useChatStore((state) => (state.activeSessionId ? state.messagesBySession[state.activeSessionId] ?? EMPTY_MESSAGES : EMPTY_MESSAGES));
  const streamingText = useChatStore((state) => (state.activeSessionId ? state.streamingBySession[state.activeSessionId] ?? '' : ''));
  const loading = useChatStore((state) => (state.activeSessionId ? state.loadingBySession[state.activeSessionId] ?? false : false));
  const thinkingStartTime = useChatStore((state) =>
    state.activeSessionId ? state.thinkingStartTimeBySession[state.activeSessionId] ?? null : null
  );

  // D5 producer: harvest `code_graph` tool-call results out of finished
  // assistant messages and feed the resolved symbol ids into the
  // `codeGraphStore.citationIds` highlight layer. Mounted at the
  // ChatView level so it survives message-list re-renders.
  useChatToolCallHarvest({ projectSlug });

  const [abortController, setAbortController] = useState<AbortController | null>(null);
  // Stable mount timestamp for synthetic UI-only placeholder bubbles (greeting,
  // streaming, thinking). `createdAt` is not displayed for these, so a single
  // captured value keeps render pure instead of calling Date.now() per frame.
  const [placeholderCreatedAt] = useState(() => Date.now());
  type StreamingToolCall = { name: string; input?: unknown };
  const toolCallsRef = useRef<StreamingToolCall[]>([]);
  const [toolCalls, setToolCalls] = useState<StreamingToolCall[]>([]);
  // Parallel to toolCallsRef: tool results in arrival order. The server emits
  // tool_call → tool_result in strict 1:1 order per tool, so results[i] pairs
  // with toolCallsRef.current[i]. Merged into the persisted assistant message
  // at finalize time so each toolCall carries both call and result.
  const toolResultsRef = useRef<{ output: string; success: boolean }[]>([]);
  const bottomRef = useRef<HTMLDivElement | null>(null);

  const { data: connectedModels = [] } = useQuery({ queryKey: ['provider-models-connected'], queryFn: fetchProviderModels });
  // Shared per-user selection (same query the Settings → Models tab uses).
  const { data: userSettings } = useQuery(userSettingsQueryOptions());

  // Order/filter the connected (tool_call-capable) models by the user's
  // per-user, per-role lane selection. Chat is a plan/reason activity, so it
  // follows the Plan lane only: when non-empty, show exactly those ids in
  // priority order, dropping any that aren't connected. When empty (user has
  // nothing in the plan lane), fall back to the full connected list so chat is
  // never left with zero models.
  const models = useMemo<ProviderModel[]>(() => {
    const selection = userSettings ? userSettings.lanes.plan : [];
    if (selection.length === 0) return connectedModels;
    const byId = new Map(connectedModels.map((m) => [m.id, m]));
    const ordered: ProviderModel[] = [];
    for (const id of selection) {
      const match = byId.get(id);
      if (match) ordered.push(match);
    }
    return ordered;
  }, [connectedModels, userSettings]);

  // Lazily fetch messages for the active session. The store treats this as
  // a cache seed — subsequent edits during streaming stay in memory.
  const { data: fetchedMessages } = useQuery({
    queryKey: ['chat-sessions', activeSessionId, 'messages'],
    queryFn: () => getChatSessionMessages(activeSessionId as string),
    enabled: Boolean(activeSessionId),
  });

  useEffect(() => {
    if (activeSessionId && fetchedMessages) {
      setSessionMessages(activeSessionId, fetchedMessages);
    }
  }, [activeSessionId, fetchedMessages, setSessionMessages]);

  const groupedModels = useMemo(() => {
    const groups = new Map<string, typeof models>();
    for (const model of models) {
      const providerId = model.provider_id ?? 'other';
      const current = groups.get(providerId) ?? [];
      current.push(model);
      groups.set(providerId, current);
    }
    return Array.from(groups.entries()).map(([providerId, providerModels]) => ({
      providerId,
      providerLabel: providerId.charAt(0).toUpperCase() + providerId.slice(1),
      models: providerModels,
    }));
  }, [models]);

  const modelOptions = useMemo(() => models.map((model) => model.id), [models]);
  const modelNameById = useMemo(() => {
    const map = new Map<string, string>();
    for (const model of models) {
      map.set(model.id, model.name);
    }
    return map;
  }, [models]);

  const [selectedModel, setSelectedModel] = useState<string>('unknown/model');

  useEffect(() => {
    if (modelOptions.length === 0) {
      // eslint-disable-next-line react-hooks/set-state-in-effect -- selection-sync effect: reads localStorage (impure) and persists via setSessionModel, so the default-model choice must live in an effect.
      setSelectedModel('unknown/model');
      return;
    }

    if (activeSession?.model && modelOptions.includes(activeSession.model)) {
      setSelectedModel(activeSession.model);
      return;
    }

    const persistedModel = typeof window !== 'undefined' ? window.localStorage.getItem(MODEL_STORAGE_KEY) : null;
    if (persistedModel && modelOptions.includes(persistedModel)) {
      setSelectedModel(persistedModel);
      if (activeSessionId) {
        setSessionModel(activeSessionId, persistedModel);
      }
      return;
    }

    const fallbackModel = modelOptions[0];
    setSelectedModel(fallbackModel);
    if (activeSessionId) {
      setSessionModel(activeSessionId, fallbackModel);
    }
  }, [activeSession?.model, activeSessionId, modelOptions, setSessionModel]);

  useEffect(() => {
    if (selectedModel && selectedModel !== 'unknown/model' && typeof window !== 'undefined') {
      window.localStorage.setItem(MODEL_STORAGE_KEY, selectedModel);
    }
  }, [selectedModel]);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages, streamingText, activeSessionId]);

  const send = async (text: string, attachments: ChatAttachment[] = []) => {
    // Reuse the active session id if one is open; otherwise mint a new UUID
    // the server will attach to the row it creates for this completion.
    const isNewSession = !activeSessionId;
    const sessionId = activeSessionId ?? generateSessionId();

    if (isNewSession) {
      const now = Date.now();
      upsertSession({
        id: sessionId,
        title: 'New Chat',
        projectSlug,
        model: selectedModel !== 'unknown/model' ? selectedModel : null,
        createdAt: now,
        updatedAt: now,
      });
      setActiveSession(sessionId);
    }

    if (selectedModel !== 'unknown/model') setSessionModel(sessionId, selectedModel);

    // A new user turn starts a new assistant response — wipe any citations
    // left over from the previous turn so the highlight layer doesn't carry
    // stale ids forward. (Producer for citations lands in a later task.)
    useCodeGraphStore.getState().clearCitations();

    addMessage(sessionId, {
      id: `${Date.now()}-user`,
      role: 'user',
      content: text,
      attachments: attachments.length > 0 ? attachments : undefined,
      createdAt: Date.now(),
    });

    clearStreaming(sessionId);
    setThinkingStartTime(sessionId, Date.now());
    toolCallsRef.current = [];
    setToolCalls([]);
    toolResultsRef.current = [];
    const controller = new AbortController();
    setAbortController(controller);

    const currentMessages = useChatStore.getState().messagesBySession[sessionId] ?? [];

    await sendChatMessage(
      sessionId,
      currentMessages,
      selectedModel,
      projectSlug,
      (delta) => appendStreamingText(sessionId, delta),
      (toolName, input) => {
        toolCallsRef.current = [...toolCallsRef.current, { name: toolName, input }];
        setToolCalls(toolCallsRef.current);
      },
      () => {
        finalizeStreaming(sessionId, {
          id: `${Date.now()}-assistant`,
          role: 'assistant',
          createdAt: Date.now(),
          toolCalls: toolCallsRef.current.map((tc, idx) => {
            const result = toolResultsRef.current[idx];
            return result
              ? { name: tc.name, input: tc.input, success: result.success, result }
              : { name: tc.name, input: tc.input };
          }),
        });
        // The server persisted a new row — invalidate the sidebar list so the
        // freshly-created session (and its server-assigned timestamps) show up.
        void queryClient.invalidateQueries({ queryKey: ['chat-sessions'] });
      },
      (message) => {
        toast.error(message);
        finalizeStreaming(sessionId, {
          id: `${Date.now()}-assistant-error`,
          role: 'assistant',
          content: 'Something went wrong while generating a response.',
          createdAt: Date.now(),
          toolCalls: toolCallsRef.current.map((tc, idx) => {
            const result = toolResultsRef.current[idx];
            return result
              ? { name: tc.name, input: tc.input, success: false, result }
              : { name: tc.name, input: tc.input, success: false };
          }),
        });
      },
      {
        signal: controller.signal,
        onSessionTitle: (title) => {
          updateSessionTitle(sessionId, title);
          void queryClient.invalidateQueries({ queryKey: ['chat-sessions'] });
        },
        onToolResult: (result) => {
          toolResultsRef.current = [
            ...toolResultsRef.current,
            { output: result.output, success: result.success },
          ];
        },
        // Carry the proposal scope so the server seeds the proposal system
        // prompt + grants the proposal-editing tools on every turn.
        proposalId: activeScope?.proposalId,
        feedbackId: activeScope?.feedbackId,
      }
    );

    setAbortController(null);
  };

  const isEmpty = !activeSessionId || messages.length === 0;

  return (
    <section className="flex min-h-0 flex-1 flex-col">
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex min-h-full max-w-3xl flex-col px-4">
          <div className="flex-1 pt-4 pb-32">
            {activeScope && <ProposalChatContext scope={activeScope} />}
            {!activeScope && isEmpty ? (
              <ChatEmptyState
                onPromptClick={(prompt) => {
                  void send(prompt, []);
                }}
              />
            ) : (
              <div className="space-y-3">
                {activeScope && messages.length === 0 && !streamingText && (
                  <ChatMessageBubble
                    message={{
                      id: 'proposal-greeting',
                      role: 'assistant',
                      content: `I can help apply this feedback to **${activeScope.proposalTitle}**. Tell me what you'd like to do — for example *“apply points 1 and 3, ignore 2”* — and I'll revise the spec and resolve the feedback.`,
                      createdAt: placeholderCreatedAt,
                    }}
                  />
                )}
                {messages.map((message) => (
                  <ChatMessageBubble key={message.id} message={message} />
                ))}
                {streamingText && (
                  <ChatMessageBubble
                    message={{
                      id: 'streaming',
                      role: 'assistant',
                      content: streamingText,
                      toolCalls: toolCalls.length > 0 ? toolCalls : undefined,
                      createdAt: placeholderCreatedAt,
                    }}
                  />
                )}
                <AnimatePresence>
                  {loading && !streamingText && thinkingStartTime !== null && (
                    <motion.div
                      initial={{ opacity: 0 }}
                      animate={{ opacity: 1 }}
                      exit={{ opacity: 0 }}
                      transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
                    >
                      {toolCalls.length > 0 && (
                        <ChatMessageBubble
                          message={{
                            id: 'thinking-tools',
                            role: 'assistant',
                            content: '',
                            toolCalls: toolCalls,
                            createdAt: placeholderCreatedAt,
                          }}
                        />
                      )}
                      <div className="pl-3 pt-1">
                        <Shimmer className="text-[13px]" duration={1.5} spread={1.5}>
                          Thinking...
                        </Shimmer>
                      </div>
                    </motion.div>
                  )}
                </AnimatePresence>
                <div ref={bottomRef} />
              </div>
            )}
          </div>

          <div className="sticky bottom-0 bg-background pb-2">
            <ChatInput
        onSend={(message, attachments) => void send(message, attachments)}
        onStop={() => {
          abortController?.abort();
          if (activeSessionId) {
            clearStreaming(activeSessionId);
          }
        }}
        streaming={loading}
        draft={draft}
        onDraftChange={(text) => setDraft(activeSessionId, text)}
        selectedModel={selectedModel}
        modelNameById={modelNameById}
        groupedModels={groupedModels}
        onModelChange={(value) => {
          if (!value) return;
          setSelectedModel(value);
          if (activeSessionId) {
            setSessionModel(activeSessionId, value);
          }
        }}
      />
          </div>
        </div>
      </div>
    </section>
  );
}
