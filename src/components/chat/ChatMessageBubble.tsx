import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import type { ChatMessage } from '@/stores/chatStore';
import { Copy } from 'lucide-react';
import ReactMarkdown from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import remarkGfm from 'remark-gfm';

interface ChatMessageBubbleProps {
  message: ChatMessage;
}

export function ChatMessageBubble({ message }: ChatMessageBubbleProps) {
  const isUser = message.role === 'user';

  const handleCopy = async () => {
    await navigator.clipboard.writeText(message.content);
  };

  return (
    <div className={cn('group flex w-full', isUser ? 'justify-end' : 'justify-start')}>
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
            <Copy className="h-3.5 w-3.5" />
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
          <div className="mt-2 flex flex-wrap gap-1.5">
            {message.toolCalls.map((tool, idx) => (
              <Badge key={`${tool.name}-${idx}`} variant="secondary" className="text-[11px]">
                Used {tool.name}
              </Badge>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
