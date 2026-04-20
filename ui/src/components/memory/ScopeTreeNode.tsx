import { useState } from 'react';
import {
  Collapsible,
  CollapsibleTrigger,
  CollapsibleContent,
} from '@/components/ui/collapsible';
import { HugeiconsIcon } from '@hugeicons/react';
import {
  Folder02Icon,
  ArrowDown01Icon,
} from '@hugeicons/core-free-icons';
import { Badge } from '@/components/ui/badge';
import { cn } from '@/lib/utils';
import {
  type ScopeTreeNode as ScopeTreeNodeType,
  countTreeNotes,
  noteTypeLabel,
  relativeTime,
} from './memoryUtils';
import type { MemoryListOutputSchema } from '@/api/generated/mcp-tools.gen';

type NoteCompact = MemoryListOutputSchema.NoteCompact;

interface ScopeTreeNodeProps {
  node: ScopeTreeNodeType;
  selectedNoteId: string | null;
  onSelectNote: (note: NoteCompact) => void;
  depth?: number;
}

export function ScopeTreeNode({
  node,
  selectedNoteId,
  onSelectNote,
  depth = 0,
}: ScopeTreeNodeProps) {
  const [open, setOpen] = useState(depth < 2);
  const totalNotes = countTreeNotes(node);

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger
        className={cn(
          'flex w-full items-center gap-1.5 rounded-md px-2 py-1 text-xs transition-colors hover:bg-white/[0.04]',
        )}
        style={{ paddingLeft: `${depth * 12 + 8}px` }}
      >
        <HugeiconsIcon
          icon={ArrowDown01Icon}
          size={12}
          className={cn(
            'shrink-0 text-muted-foreground transition-transform duration-200',
            !open && '-rotate-90',
          )}
        />
        <HugeiconsIcon
          icon={Folder02Icon}
          size={13}
          className="shrink-0 text-muted-foreground"
        />
        <span className="truncate flex-1 text-left text-muted-foreground">
          {node.segment}
        </span>
        <span className="shrink-0 text-[10px] text-muted-foreground/60">
          {totalNotes}
        </span>
      </CollapsibleTrigger>
      <CollapsibleContent className="overflow-hidden">
        {node.notes.map((note) => (
          <NoteRow
            key={note.id}
            note={note}
            isSelected={selectedNoteId === note.id}
            onSelect={onSelectNote}
            indent={depth + 1}
          />
        ))}
        {node.children.map((child) => (
          <ScopeTreeNode
            key={child.fullPath}
            node={child}
            selectedNoteId={selectedNoteId}
            onSelectNote={onSelectNote}
            depth={depth + 1}
          />
        ))}
      </CollapsibleContent>
    </Collapsible>
  );
}

function NoteRow({
  note,
  isSelected,
  onSelect,
  indent,
}: {
  note: NoteCompact;
  isSelected: boolean;
  onSelect: (note: NoteCompact) => void;
  indent: number;
}) {
  return (
    <button
      type="button"
      onClick={() => onSelect(note)}
      className={cn(
        'flex w-full items-center gap-2 rounded-md px-2 py-1 text-xs transition-colors',
        isSelected
          ? 'bg-white/[0.07] text-foreground'
          : 'text-muted-foreground hover:bg-white/[0.04] hover:text-foreground',
      )}
      style={{ paddingLeft: `${indent * 12 + 20}px` }}
    >
      <Badge variant="secondary" className="h-4 px-1.5 text-[10px] shrink-0">
        {noteTypeLabel(note.note_type)}
      </Badge>
      <span className="truncate flex-1 text-left">{note.title}</span>
      <span className="shrink-0 text-[10px] text-muted-foreground/60">
        {relativeTime(note.updated_at)}
      </span>
    </button>
  );
}
