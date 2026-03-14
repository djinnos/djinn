import { useEffect, useMemo, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { Select, SelectContent, SelectGroup, SelectItem, SelectLabel, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Square, ArrowUp } from 'lucide-react';
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
  prefillValue?: string;
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
  prefillValue,
  selectedModel,
  modelNameById,
  groupedModels,
  onModelChange,
}: ChatInputProps) {
  const [value, setValue] = useState(prefillValue ?? '');
  const [textareaHeight, setTextareaHeight] = useState(44);
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
    const nextHeight = Math.min(el.scrollHeight, maxHeight);
    setTextareaHeight(nextHeight);
    el.style.height = `${nextHeight}px`;
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
      <motion.div
        layout
        transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
        className="relative rounded-xl border border-input bg-background"
      >
        <motion.div
          animate={{ height: textareaHeight }}
          transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
          className="overflow-hidden"
        >
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
            className="min-h-[44px] max-h-[144px] resize-none border-0 bg-transparent pr-12 shadow-none focus-visible:ring-0"
          />
        </motion.div>
        <Button
          type="button"
          size="icon"
          variant={streaming ? 'outline' : 'default'}
          onClick={streaming ? onStop : handleSend}
          disabled={!streaming && !canSend}
          className="absolute bottom-2 right-2 h-8 w-8 rounded-full"
        >
          {streaming ? <Square className="h-3.5 w-3.5" /> : <ArrowUp className="h-3.5 w-3.5" />}
        </Button>
      </motion.div>
      <div className="mt-2 flex items-center justify-between gap-2 px-1">
        <Select value={selectedModel} onValueChange={onModelChange}>
          <SelectTrigger className="h-8 w-auto min-w-0 gap-1 border-0 px-1 text-xs shadow-none focus:ring-0">
            <SelectValue placeholder="Select a model">
              {selectedModel !== 'unknown/model' ? modelNameById.get(selectedModel) ?? selectedModel : undefined}
            </SelectValue>
          </SelectTrigger>
          <SelectContent>
            {groupedModels.map((group) => (
              <SelectGroup key={group.providerId}>
                <SelectLabel>{group.providerLabel}</SelectLabel>
                {group.models.map((model) => (
                  <SelectItem key={model.id} value={model.id}>
                    {model.name}
                  </SelectItem>
                ))}
              </SelectGroup>
            ))}
          </SelectContent>
        </Select>
        <div />
      </div>
    </div>
  );
}
