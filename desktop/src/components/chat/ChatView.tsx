import { useEffect, useMemo, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { fetchProviderModels } from '@/api/settings';
import { sendChatMessage } from '@/api/chat';
import { Spinner } from '@/components/ui/spinner';
import { toast } from '@/lib/toast';
import { useChatStore, type ChatMessage } from '@/stores/chatStore';
import { useIsAllProjects, useSelectedProject } from '@/stores/useProjectStore';
import { ChatMessageBubble } from './ChatMessageBubble';
import { ChatInput } from './ChatInput';
import { ChatEmptyState } from './ChatEmptyState';
import { AnimatePresence, motion } from 'framer-motion';

const EMPTY_MESSAGES: ChatMessage[] = [];
const MODEL_STORAGE_KEY = 'djinnos-chat-model';

export function ChatView() {
  const isAllProjects = useIsAllProjects();
  const selectedProject = useSelectedProject();
  const projectPath = isAllProjects ? null : (selectedProject?.path ?? null);

  const createSession = useChatStore((state) => state.createSession);
  const activeSessionId = useChatStore((state) => state.activeSessionId);
  const activeSession = useChatStore((state) =>
    state.activeSessionId ? state.sessions.find((session) => session.id === state.activeSessionId) ?? null : null
  );
  const setActiveSession = useChatStore((state) => state.setActiveSession);
  const setSessionModel = useChatStore((state) => state.setSessionModel);
  const addMessage = useChatStore((state) => state.addMessage);
  const appendStreamingText = useChatStore((state) => state.appendStreamingText);
  const finalizeStreaming = useChatStore((state) => state.finalizeStreaming);
  const updateSessionTitle = useChatStore((state) => state.updateSessionTitle);
  const clearStreaming = useChatStore((state) => state.clearStreaming);
  const setThinkingStartTime = useChatStore((state) => state.setThinkingStartTime);
  const messages = useChatStore((state) => (state.activeSessionId ? state.messagesBySession[state.activeSessionId] ?? EMPTY_MESSAGES : EMPTY_MESSAGES));
  const streamingText = useChatStore((state) => (state.activeSessionId ? state.streamingBySession[state.activeSessionId] ?? '' : ''));
  const loading = useChatStore((state) => (state.activeSessionId ? state.loadingBySession[state.activeSessionId] ?? false : false));
  const thinkingStartTime = useChatStore((state) =>
    state.activeSessionId ? state.thinkingStartTimeBySession[state.activeSessionId] ?? null : null
  );

  const [promptSeed, setPromptSeed] = useState<string | undefined>(undefined);
  const [abortController, setAbortController] = useState<AbortController | null>(null);
  const [toolCalls, setToolCalls] = useState<string[]>([]);
  const [elapsedSeconds, setElapsedSeconds] = useState(0);
  const bottomRef = useRef<HTMLDivElement | null>(null);

  const { data: models = [] } = useQuery({ queryKey: ['provider-models-connected'], queryFn: fetchProviderModels });

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

  useEffect(() => {
    const shouldShowThinking = loading && !streamingText && thinkingStartTime !== null;

    if (!shouldShowThinking || thinkingStartTime === null) {
      setElapsedSeconds(0);
      return;
    }

    setElapsedSeconds(Math.floor((Date.now() - thinkingStartTime) / 1000));

    const intervalId = window.setInterval(() => {
      setElapsedSeconds(Math.floor((Date.now() - thinkingStartTime) / 1000));
    }, 1000);

    return () => {
      window.clearInterval(intervalId);
    };
  }, [loading, streamingText, thinkingStartTime]);

  const send = async (text: string) => {
    const sessionId = activeSessionId ?? createSession(projectPath, selectedModel !== 'unknown/model' ? selectedModel : null);
    if (!activeSessionId) setActiveSession(sessionId);
    if (selectedModel !== 'unknown/model') setSessionModel(sessionId, selectedModel);

    addMessage(sessionId, {
      id: `${Date.now()}-user`,
      role: 'user',
      content: text,
      createdAt: Date.now(),
    });

    clearStreaming(sessionId);
    setThinkingStartTime(sessionId, Date.now());
    setToolCalls([]);
    const controller = new AbortController();
    setAbortController(controller);

    const currentMessages = useChatStore.getState().messagesBySession[sessionId] ?? [];

    await sendChatMessage(
      currentMessages,
      selectedModel,
      projectPath,
      (delta) => appendStreamingText(sessionId, delta),
      (toolName) => setToolCalls((prev) => [...prev, toolName]),
      () => {
        finalizeStreaming(sessionId, {
          id: `${Date.now()}-assistant`,
          role: 'assistant',
          createdAt: Date.now(),
          toolCalls: toolCalls.map((name) => ({ name })),
        });

        const state = useChatStore.getState();
        const session = state.sessions.find((s) => s.id === sessionId);
        const sessionMessages = state.messagesBySession[sessionId] ?? [];
        const firstUserMessage = sessionMessages.find((m) => m.role === 'user');
        const firstAssistantMessage = sessionMessages.find((m) => m.role === 'assistant');

        if (
          session?.title === 'New Chat' &&
          firstUserMessage &&
          firstAssistantMessage
        ) {
          void sendChatMessage(
            [
              { ...firstUserMessage, content: firstUserMessage.content },
              { ...firstAssistantMessage, content: firstAssistantMessage.content },
            ],
            selectedModel,
            projectPath,
            () => {},
            () => {},
            () => {},
            () => {},
            {
              systemPrompt:
                'Generate a concise 3-6 word title for this conversation. Return only the title text, nothing else.',
              onCompleteText: (titleText) => updateSessionTitle(sessionId, titleText),
            }
          );
        }
      },
      (message) => {
        toast.error(message);
        finalizeStreaming(sessionId, {
          id: `${Date.now()}-assistant-error`,
          role: 'assistant',
          content: 'Something went wrong while generating a response.',
          createdAt: Date.now(),
          toolCalls: toolCalls.map((name) => ({ name, success: false })),
        });
      },
      { signal: controller.signal }
    );

    setAbortController(null);
  };

  const isEmpty = !activeSessionId || messages.length === 0;

  return (
    <section className="flex min-h-0 flex-1 flex-col">
      <header className="border-b border-border px-4 py-3 text-sm text-muted-foreground">
        {activeSession?.title ?? 'New Chat'}
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto p-4">
        {isEmpty ? (
          <ChatEmptyState
            onPromptClick={(prompt) => {
              setPromptSeed(prompt);
              void send(prompt);
            }}
          />
        ) : (
          <div className="space-y-3">
            {messages.map((message) => (
              <ChatMessageBubble key={message.id} message={message} />
            ))}
            {streamingText && (
              <ChatMessageBubble
                message={{
                  id: 'streaming',
                  role: 'assistant',
                  content: streamingText,
                  createdAt: Date.now(),
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
                  className="pl-3"
                >
                  <div className="inline-flex items-center gap-2 text-[13px] text-muted-foreground/70">
                    <Spinner size="xs" />
                    <span>Thinking...</span>
                    <span>{elapsedSeconds}s</span>
                  </div>
                </motion.div>
              )}
            </AnimatePresence>
            <div ref={bottomRef} />
          </div>
        )}
      </div>

      <ChatInput
        onSend={(message) => void send(message)}
        onStop={() => {
          abortController?.abort();
          if (activeSessionId) {
            clearStreaming(activeSessionId);
          }
        }}
        streaming={loading}
        prefillValue={promptSeed}
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
    </section>
  );
}
