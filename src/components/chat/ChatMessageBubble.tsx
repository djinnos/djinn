import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import type { ChatMessage } from '@/stores/chatStore';

interface ChatMessageBubbleProps {
  message: ChatMessage;
}

function renderMarkdown(text: string): string {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/\n/g, '<br />');
}

export function ChatMessageBubble({ message }: ChatMessageBubbleProps) {
  const isUser = message.role === 'user';

  return (
    <div className={cn('flex w-full', isUser ? 'justify-end' : 'justify-start')}>
      <div
        className={cn(
          'max-w-[80%] rounded-lg px-4 py-3 text-sm',
          isUser ? 'bg-muted text-foreground' : 'bg-card text-foreground border border-border'
        )}
      >
        {isUser ? (
          <p className="whitespace-pre-wrap break-words">{message.content}</p>
        ) : (
          <div
            className="prose prose-sm prose-invert max-w-none break-words"
            dangerouslySetInnerHTML={{ __html: renderMarkdown(message.content) }}
          />
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
