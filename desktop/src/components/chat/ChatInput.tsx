import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { ModelSelector } from './ModelSelector';
import {
  Attachment,
  AttachmentHoverCard,
  AttachmentHoverCardContent,
  AttachmentHoverCardTrigger,
  AttachmentInfo,
  AttachmentPreview,
  AttachmentRemove,
  Attachments,
  getAttachmentLabel,
  getMediaCategory,
  type AttachmentData,
} from '@/components/ai-elements/attachments';
import type { ChatAttachment } from '@/stores/chatStore';

import { Attachment01Icon, Sent02Icon, StopIcon } from '@hugeicons/core-free-icons';
import { HugeiconsIcon } from '@hugeicons/react';
import { motion } from 'framer-motion';

const MAX_FILE_SIZE = 20 * 1024 * 1024; // 20 MB

interface ModelGroup {
  providerId: string;
  providerLabel: string;
  models: { id: string; name: string }[];
}

interface ChatInputProps {
  onSend: (message: string, attachments: ChatAttachment[]) => void;
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

function generateId(): string {
  return crypto.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(36).slice(2, 10)}`;
}

function readFileAsBase64(file: File): Promise<{ data: string; url: string }> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result as string;
      // result is "data:<mediaType>;base64,<data>"
      const commaIdx = result.indexOf(',');
      resolve({
        data: result.slice(commaIdx + 1),
        url: result,
      });
    };
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
}

async function filesToAttachments(files: FileList | File[]): Promise<ChatAttachment[]> {
  const result: ChatAttachment[] = [];
  for (const file of files) {
    if (file.size > MAX_FILE_SIZE) continue;
    const { data, url } = await readFileAsBase64(file);
    result.push({
      id: generateId(),
      filename: file.name,
      mediaType: file.type || 'application/octet-stream',
      data,
      url: file.type.startsWith('image/') ? url : undefined,
    });
  }
  return result;
}

function toAttachmentData(att: ChatAttachment): AttachmentData {
  return {
    type: 'file' as const,
    id: att.id,
    mediaType: att.mediaType,
    filename: att.filename,
    url: att.url ?? '',
  };
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
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [attachments, setAttachments] = useState<ChatAttachment[]>([]);
  const [dragOver, setDragOver] = useState(false);

  const canSend = useMemo(
    () => (draft.trim().length > 0 || attachments.length > 0) && !streaming,
    [draft, streaming, attachments.length]
  );

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
    if ((!trimmed && attachments.length === 0) || streaming) return;
    onSend(trimmed, attachments);
    onDraftChange('');
    setAttachments([]);
  };

  const handleRemove = useCallback((id: string) => {
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  }, []);

  const addFiles = useCallback(async (files: FileList | File[]) => {
    const newAttachments = await filesToAttachments(files);
    if (newAttachments.length > 0) {
      setAttachments((prev) => [...prev, ...newAttachments]);
    }
  }, []);

  const handlePaste = useCallback(
    (e: React.ClipboardEvent) => {
      const items = e.clipboardData?.items;
      if (!items) return;
      const files: File[] = [];
      for (const item of items) {
        if (item.kind === 'file') {
          const file = item.getAsFile();
          if (file) files.push(file);
        }
      }
      if (files.length > 0) {
        e.preventDefault();
        void addFiles(files);
      }
    },
    [addFiles]
  );

  const handleDrop = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      setDragOver(false);
      if (e.dataTransfer?.files?.length) {
        void addFiles(e.dataTransfer.files);
      }
    },
    [addFiles]
  );

  const handleDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    setDragOver(true);
  }, []);

  const handleDragLeave = useCallback((e: React.DragEvent) => {
    e.preventDefault();
    setDragOver(false);
  }, []);

  return (
    <div className="w-full pt-2">
      <motion.div
        layout
        transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
        className={`relative rounded-t-xl bg-input/30 pt-2 px-2 transition-colors ${
          dragOver ? 'ring-2 ring-primary/40 bg-primary/5' : ''
        }`}
        onDrop={handleDrop}
        onDragOver={handleDragOver}
        onDragLeave={handleDragLeave}
      >
        {attachments.length > 0 && (
          <div className="px-2 pt-1 pb-1">
            <Attachments variant="inline">
              {attachments.map((att) => {
                const attData = toAttachmentData(att);
                const mediaCategory = getMediaCategory(attData);
                const label = getAttachmentLabel(attData);
                return (
                  <AttachmentHoverCard key={att.id}>
                    <AttachmentHoverCardTrigger>
                      <Attachment data={attData} onRemove={() => handleRemove(att.id)}>
                        <div className="relative size-5 shrink-0">
                          <div className="absolute inset-0 transition-opacity group-hover:opacity-0">
                            <AttachmentPreview />
                          </div>
                          <AttachmentRemove className="absolute inset-0" />
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

        <motion.div
          animate={{ height: textareaHeight }}
          transition={{ duration: 0.2, ease: [0.4, 0, 0.2, 1] }}
          className="overflow-hidden"
        >
          <Textarea
            ref={textareaRef}
            value={draft}
            onChange={(event) => onDraftChange(event.target.value)}
            onPaste={handlePaste}
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
          <div className="flex items-center gap-1">
            <ModelSelector
              selectedModel={selectedModel}
              modelNameById={modelNameById}
              groupedModels={groupedModels}
              onModelChange={onModelChange}
            />
            <input
              ref={fileInputRef}
              type="file"
              multiple
              className="hidden"
              accept="image/*,application/pdf,.txt,.csv,.json,.xml,.md,.html"
              onChange={(e) => {
                if (e.target.files?.length) {
                  void addFiles(e.target.files);
                  e.target.value = '';
                }
              }}
            />
            <Button
              type="button"
              size="icon"
              variant="ghost"
              className="h-8 w-8 rounded-lg text-muted-foreground hover:text-foreground"
              onClick={() => fileInputRef.current?.click()}
              aria-label="Attach file"
            >
              <HugeiconsIcon icon={Attachment01Icon} size={16} />
            </Button>
          </div>
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
