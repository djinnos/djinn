import { cn } from '@/lib/utils';
import type { ChatAttachment, ChatMessage } from '@/stores/chatStore';
import {
  Attachment,
  AttachmentHoverCard,
  AttachmentHoverCardContent,
  AttachmentHoverCardTrigger,
  AttachmentInfo,
  AttachmentPreview,
  Attachments,
  getAttachmentLabel,
  getMediaCategory,
  type AttachmentData,
} from '@/components/ai-elements/attachments';
import { ArrowRight01Icon, Copy01Icon, Tick02Icon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { AnimatePresence, motion } from 'framer-motion';
import { Streamdown } from 'streamdown';
import { useState } from 'react';

function toAttachmentData(att: ChatAttachment): AttachmentData {
  return {
    type: 'file' as const,
    id: att.id,
    mediaType: att.mediaType,
    filename: att.filename,
    url: att.url ?? '',
  };
}

interface ChatMessageBubbleProps {
  message: ChatMessage;
}

export function ChatMessageBubble({ message }: ChatMessageBubbleProps) {
  const isUser = message.role === 'user';
  const [toolCallsExpanded, setToolCallsExpanded] = useState(false);
  const [copied, setCopied] = useState(false);

  const handleCopy = async () => {
    await navigator.clipboard.writeText(message.content);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
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
            : 'text-foreground'
        )}
      >
        {isUser && (
          <button
            type="button"
            onClick={handleCopy}
            aria-label="Copy message"
            className={cn(
              'absolute -top-2 -right-2 rounded-md border border-border bg-background/95 p-1 shadow-sm transition-opacity',
              copied ? 'text-green-500 opacity-100' : 'text-muted-foreground opacity-0 hover:text-foreground group-hover:opacity-100'
            )}
          >
            <HugeiconsIcon icon={copied ? Tick02Icon : Copy01Icon} size={14} />
          </button>
        )}
        {!isUser && message.toolCalls && message.toolCalls.length > 0 && (
          <div className="mb-2 rounded-md border border-border/60 bg-muted/20">
            <button
              type="button"
              onClick={() => setToolCallsExpanded((prev) => !prev)}
              className="flex w-full items-center justify-between px-2 py-1.5 text-left text-[11px] text-muted-foreground hover:text-foreground"
            >
              <span>Used {message.toolCalls.length} tool{message.toolCalls.length !== 1 ? 's' : ''}</span>
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
                  <div className="flex flex-col gap-0.5 px-2 pb-2">
                    {message.toolCalls.map((tool, idx) => (
                      <div key={`${tool.name}-${idx}`} className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
                        <span className={cn(
                          'size-1.5 shrink-0 rounded-full',
                          tool.success === false ? 'bg-red-400' : 'bg-emerald-400'
                        )} />
                        <span className="truncate">{tool.name}</span>
                      </div>
                    ))}
                  </div>
                </motion.div>
              )}
            </AnimatePresence>
          </div>
        )}

        {isUser && message.attachments && message.attachments.length > 0 && (
          <div className="mb-2">
            <Attachments variant="inline">
              {message.attachments.map((att) => {
                const attData = toAttachmentData(att);
                const mediaCategory = getMediaCategory(attData);
                const label = getAttachmentLabel(attData);
                return (
                  <AttachmentHoverCard key={att.id}>
                    <AttachmentHoverCardTrigger>
                      <Attachment data={attData}>
                        <div className="relative size-5 shrink-0">
                          <AttachmentPreview />
                        </div>
                        <AttachmentInfo />
                      </Attachment>
                    </AttachmentHoverCardTrigger>
                    <AttachmentHoverCardContent>
                      <div className="space-y-3">
                        {mediaCategory === 'image' && att.url && (
                          <div className="flex max-h-96 w-80 items-center justify-center overflow-hidden rounded-md border">
                            <img
                              alt={label}
                              className="max-h-full max-w-full object-contain"
                              height={384}
                              src={att.url}
                              width={320}
                            />
                          </div>
                        )}
                        <div className="space-y-1 px-0.5">
                          <h4 className="font-semibold text-sm leading-none">{label}</h4>
                          {att.mediaType && (
                            <p className="font-mono text-muted-foreground text-xs">{att.mediaType}</p>
                          )}
                        </div>
                      </div>
                    </AttachmentHoverCardContent>
                  </AttachmentHoverCard>
                );
              })}
            </Attachments>
          </div>
        )}

        {isUser ? (
          <p className="whitespace-pre-wrap break-words">{message.content}</p>
        ) : (
          <Streamdown className="prose prose-sm max-w-none break-words dark:prose-invert">
            {message.content}
          </Streamdown>
        )}

        {!isUser && (
          <div className="mt-1 flex opacity-0 transition-opacity group-hover:opacity-100">
            <button
              type="button"
              onClick={handleCopy}
              aria-label="Copy message"
              className={cn(
                'rounded-md p-1',
                copied ? 'text-green-500' : 'text-muted-foreground hover:text-foreground'
              )}
            >
              <HugeiconsIcon icon={copied ? Tick02Icon : Copy01Icon} size={14} />
            </button>
          </div>
        )}
      </div>
    </motion.div>
  );
}
