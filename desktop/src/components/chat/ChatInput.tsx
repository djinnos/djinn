import { useEffect, useMemo, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { ModelSelector } from './ModelSelector';

import { Sent02Icon, StopIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { motion } from 'framer-motion';

interface ModelGroup {
  providerId: string;
  providerLabel: string;
  models: { id: string; name: string }[];
}

interface ChatInputProps {
  onSend: (message: string) => void;
  onStop: () => void;
  streaming: boolean;
  placeholder?: string;
  draft: string;
  onDraftChange: (text: string) => void;
  selectedModel: string;
  modelNameById: Map<string, string>;
  groupedModels: ModelGroup[];
  onModelChange: (value: string | null) => void;
}

export function ChatInput({
  onSend,
  onStop,
  streaming,
  placeholder = 'Ask Djinn…',
  draft,
  onDraftChange,
  selectedModel,
  modelNameById,
  groupedModels,
  onModelChange,
}: ChatInputProps) {
  const [textareaHeight, setTextareaHeight] = useState(56);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  const canSend = useMemo(() => draft.trim().length > 0 && !streaming, [draft, streaming]);

  const resizeTextarea = () => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    const lineHeight = 24;
    const maxHeight = lineHeight * 6;
    const nextHeight = Math.min(el.scrollHeight, maxHeight);
    setTextareaHeight(nextHeight);
    el.style.height = `${nextHeight}px`;
    el.style.overflowY = el.scrollHeight > maxHeight ? 'auto' : 'hidden';
  };

  useEffect(() => {
    resizeTextarea();
  }, [draft]);

  const handleSend = () => {
    const trimmed = draft.trim();
    if (!trimmed || streaming) return;
    onSend(trimmed);
    onDraftChange('');
  };

  return (
    <div className="w-full pt-2">
      <motion.div
        layout
        transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
        className="relative rounded-t-xl bg-input/30 pt-2 px-2"
      >
        <motion.div
          animate={{ height: textareaHeight }}
          transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
          className="overflow-hidden"
        >
          <Textarea
            ref={textareaRef}
            value={draft}
            onChange={(event) => onDraftChange(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey && !event.metaKey && !event.ctrlKey) {
                event.preventDefault();
                handleSend();
              } else if (event.key === 'Enter' && (event.ctrlKey || event.shiftKey)) {
                event.preventDefault();
                const start = event.currentTarget.selectionStart;
                const end = event.currentTarget.selectionEnd;
                const newValue = draft.slice(0, start) + '\n' + draft.slice(end);
                onDraftChange(newValue);
                requestAnimationFrame(() => {
                  if (textareaRef.current) {
                    textareaRef.current.selectionStart = start + 1;
                    textareaRef.current.selectionEnd = start + 1;
                  }
                });
              }
            }}
            placeholder={placeholder}
            className="min-h-[56px] max-h-[144px] resize-none border-0 bg-transparent pr-12 shadow-none focus-visible:ring-0 dark:bg-transparent"
          />
        </motion.div>
        <div className="flex items-center justify-between px-2 pb-2">
          <ModelSelector
            selectedModel={selectedModel}
            modelNameById={modelNameById}
            groupedModels={groupedModels}
            onModelChange={onModelChange}
          />
          <Button
            type="button"
            size="icon"
            variant={streaming ? 'outline' : 'default'}
            onClick={streaming ? onStop : handleSend}
            disabled={!streaming && !canSend}
            className="h-8 w-8 rounded-lg"
          >
            {streaming ? <HugeiconsIcon icon={StopIcon} size={14} /> : <HugeiconsIcon icon={Sent02Icon} size={14} />}
          </Button>
        </div>
      </motion.div>
    </div>
  );
}
