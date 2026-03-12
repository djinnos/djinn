import { useEffect, useMemo, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { Square, Send } from 'lucide-react';

interface ChatInputProps {
  onSend: (message: string) => void;
  onStop: () => void;
  streaming: boolean;
  placeholder?: string;
  prefillValue?: string;
}

export function ChatInput({ onSend, onStop, streaming, placeholder = 'Ask Djinn…', prefillValue }: ChatInputProps) {
  const [value, setValue] = useState(prefillValue ?? '');
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    if (prefillValue !== undefined) {
      setValue(prefillValue);
    }
  }, [prefillValue]);

  const canSend = useMemo(() => value.trim().length > 0 && !streaming, [value, streaming]);

  const resizeTextarea = () => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = 'auto';
    const lineHeight = 24;
    const maxHeight = lineHeight * 6;
    el.style.height = `${Math.min(el.scrollHeight, maxHeight)}px`;
    el.style.overflowY = el.scrollHeight > maxHeight ? 'auto' : 'hidden';
  };

  useEffect(() => {
    resizeTextarea();
  }, [value]);

  const handleSend = () => {
    const trimmed = value.trim();
    if (!trimmed || streaming) return;
    onSend(trimmed);
    setValue('');
  };

  return (
    <div className="border-t border-border p-3">
      <div className="flex items-end gap-2">
        <Textarea
          ref={textareaRef}
          value={value}
          onChange={(event) => setValue(event.target.value)}
          onKeyDown={(event) => {
            if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
              event.preventDefault();
              handleSend();
            }
          }}
          placeholder={placeholder}
          className="min-h-[44px] max-h-[144px] resize-none"
          disabled={streaming}
        />
        {streaming ? (
          <Button type="button" variant="outline" onClick={onStop}>
            <Square className="h-4 w-4" />
          </Button>
        ) : (
          <Button type="button" onClick={handleSend} disabled={!canSend}>
            <Send className="h-4 w-4" />
          </Button>
        )}
      </div>
    </div>
  );
}
