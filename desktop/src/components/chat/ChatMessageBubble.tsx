import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import type { ChatMessage } from '@/stores/chatStore';
import { ArrowRight01Icon, Copy01Icon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { AnimatePresence, motion } from 'framer-motion';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import remarkGfm from 'remark-gfm';
import { useState } from 'react';

interface ChatMessageBubbleProps {
  message: ChatMessage;
}

export function ChatMessageBubble({ message }: ChatMessageBubbleProps) {
  const isUser = message.role === 'user';
  const [toolCallsExpanded, setToolCallsExpanded] = useState(true);

  const handleCopy = async () => {
    await navigator.clipboard.writeText(message.content);
  };

  return (
    <motion.div
      initial={{ opacity: 0, y: 8 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
      className={cn('group flex w-full', isUser ? 'justify-end' : 'justify-start')}
    >
      <div
        className={cn(
          'relative max-w-[80%] px-4 py-3 text-sm',
          isUser
            ? 'rounded-2xl bg-foreground/5 text-foreground'
            : 'rounded-lg border border-border bg-card text-foreground'
        )}
      >
        {isUser && (
          <button
            type="button"
            onClick={handleCopy}
            aria-label="Copy message"
            className="absolute -top-2 -right-2 rounded-md border border-border bg-background/95 p-1 text-muted-foreground opacity-0 shadow-sm transition-opacity hover:text-foreground group-hover:opacity-100"
          >
            <HugeiconsIcon icon={Copy01Icon} size={14} />
          </button>
        )}
        {isUser ? (
          <p className="whitespace-pre-wrap break-words">{message.content}</p>
        ) : (
          <div className="prose prose-sm max-w-none break-words dark:prose-invert">
            <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}>
              {message.content}
            </ReactMarkdown>
          </div>
        )}

        {!isUser && message.toolCalls && message.toolCalls.length > 0 && (
          <div className="mt-2 rounded-md border border-border/60 bg-muted/20">
            <button
              type="button"
              onClick={() => setToolCallsExpanded((prev) => !prev)}
              className="flex w-full items-center justify-between px-2 py-1.5 text-left text-[11px] text-muted-foreground hover:text-foreground"
            >
              <span>Tool calls ({message.toolCalls.length})</span>
              <motion.span
                animate={{ rotate: toolCallsExpanded ? 90 : 0 }}
                transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
                className="inline-flex"
              >
                <HugeiconsIcon icon={ArrowRight01Icon} size={14} />
              </motion.span>
            </button>

            <AnimatePresence initial={false}>
              {toolCallsExpanded && (
                <motion.div
                  key="tool-calls"
                  initial={{ height: 0, opacity: 0 }}
                  animate={{ height: 'auto', opacity: 1 }}
                  exit={{ height: 0, opacity: 0 }}
                  transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
                  className="overflow-hidden"
                >
                  <div className="flex flex-wrap gap-1.5 px-2 pb-2">
                    {message.toolCalls.map((tool, idx) => (
                      <Badge key={`${tool.name}-${idx}`} variant="secondary" className="text-[11px]">
                        Used {tool.name}
                      </Badge>
                    ))}
                  </div>
                </motion.div>
              )}
            </AnimatePresence>
          </div>
        )}
      </div>
    </motion.div>
  );
}
