import { useState, useMemo } from 'react';
import {
  Collapsible,
  CollapsibleTrigger,
  CollapsibleContent,
} from '@/components/ui/collapsible';
import { Input } from '@/components/ui/input';
import { Badge } from '@/components/ui/badge';
import { HugeiconsIcon } from '@hugeicons/react';
import {
  ArrowDown01Icon,
  Search01Icon,
} from '@hugeicons/core-free-icons';
import { cn } from '@/lib/utils';
import { ScopeTreeNode } from './ScopeTreeNode';
import {
  partitionNotes,
  groupByType,
  sortedTypeEntries,
  buildScopeTree,
  noteTypeLabel,
  relativeTime,
} from './memoryUtils';
import type { MemoryListOutputSchema, MemorySearchOutputSchema, MemoryHealthOutput } from '@/api/generated/mcp-tools.gen';

type NoteCompact = MemoryListOutputSchema.NoteCompact;
type SearchResult = MemorySearchOutputSchema.MemorySearchResultItem;

interface MemoryExplorerProps {
  notes: NoteCompact[];
  searchQuery: string;
  onSearchChange: (query: string) => void;
  searchResults: SearchResult[] | null;
  selectedNoteId: string | null;
  onSelectNote: (note: NoteCompact | SearchResult) => void;
  health: MemoryHealthOutput | null;
}

export function MemoryExplorer({
  notes,
  searchQuery,
  onSearchChange,
  searchResults,
  selectedNoteId,
  onSelectNote,
  health,
}: MemoryExplorerProps) {
  const { global, scoped } = useMemo(() => partitionNotes(notes), [notes]);
  const globalGrouped = useMemo(() => sortedTypeEntries(groupByType(global)), [global]);
  const scopeTree = useMemo(() => buildScopeTree(scoped), [scoped]);

  const isSearching = searchQuery.length > 0;

  return (
    <aside className="flex w-80 shrink-0 flex-col border-r border-border">
      {/* Search */}
      <div className="shrink-0 border-b border-border p-3">
        <div className="relative">
          <HugeiconsIcon
            icon={Search01Icon}
            size={14}
            className="absolute left-2.5 top-1/2 -translate-y-1/2 text-muted-foreground"
          />
          <Input
            value={searchQuery}
            onChange={(e) => onSearchChange(e.target.value)}
            placeholder="Search notes..."
            className="pl-8"
          />
        </div>
        {health && (
          <p className="mt-2 text-[11px] text-muted-foreground">
            {health.total_notes ?? 0} notes
            {(health.broken_link_count ?? 0) > 0 && (
              <span className="text-amber-400"> · {health.broken_link_count} broken links</span>
            )}
            {(health.orphan_note_count ?? 0) > 0 && (
              <span> · {health.orphan_note_count} orphans</span>
            )}
          </p>
        )}
      </div>

      {/* Content */}
      <div className="flex-1 overflow-y-auto p-2 space-y-1">
        {isSearching ? (
          <SearchResultsList
            results={searchResults}
            selectedNoteId={selectedNoteId}
            onSelectNote={onSelectNote}
          />
        ) : (
          <>
            <GlobalSection
              grouped={globalGrouped}
              totalCount={global.length}
              selectedNoteId={selectedNoteId}
              onSelectNote={onSelectNote}
            />
            <ScopedSection
              tree={scopeTree}
              totalCount={scoped.length}
              selectedNoteId={selectedNoteId}
              onSelectNote={onSelectNote}
            />
          </>
        )}
      </div>
    </aside>
  );
}

function GlobalSection({
  grouped,
  totalCount,
  selectedNoteId,
  onSelectNote,
}: {
  grouped: [string, NoteCompact[]][];
  totalCount: number;
  selectedNoteId: string | null;
  onSelectNote: (note: NoteCompact) => void;
}) {
  const [open, setOpen] = useState(true);

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-xs font-medium transition-colors hover:bg-white/[0.04]">
        <HugeiconsIcon
          icon={ArrowDown01Icon}
          size={12}
          className={cn(
            'shrink-0 text-muted-foreground transition-transform duration-200',
            !open && '-rotate-90',
          )}
        />
        <span className="flex-1 text-left text-foreground">Global</span>
        <span className="text-[10px] text-muted-foreground/60">{totalCount}</span>
      </CollapsibleTrigger>
      <CollapsibleContent className="overflow-hidden">
        {grouped.length === 0 ? (
          <p className="px-4 py-2 text-[11px] text-muted-foreground/60">No global notes</p>
        ) : (
          grouped.map(([type, notes]) => (
            <TypeGroup
              key={type}
              type={type}
              notes={notes}
              selectedNoteId={selectedNoteId}
              onSelectNote={onSelectNote}
            />
          ))
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}

function TypeGroup({
  type,
  notes,
  selectedNoteId,
  onSelectNote,
}: {
  type: string;
  notes: NoteCompact[];
  selectedNoteId: string | null;
  onSelectNote: (note: NoteCompact) => void;
}) {
  const [open, setOpen] = useState(false);

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger className="flex w-full items-center gap-1.5 rounded-md px-2 py-1 pl-6 text-xs transition-colors hover:bg-white/[0.04]">
        <HugeiconsIcon
          icon={ArrowDown01Icon}
          size={11}
          className={cn(
            'shrink-0 text-muted-foreground transition-transform duration-200',
            !open && '-rotate-90',
          )}
        />
        <span className="truncate flex-1 text-left text-muted-foreground">
          {noteTypeLabel(type)}
        </span>
        <span className="shrink-0 text-[10px] text-muted-foreground/60">{notes.length}</span>
      </CollapsibleTrigger>
      <CollapsibleContent className="overflow-hidden">
        {notes.map((note) => (
          <button
            key={note.id}
            type="button"
            onClick={() => onSelectNote(note)}
            className={cn(
              'flex w-full items-center gap-2 rounded-md px-2 py-1 pl-10 text-xs transition-colors',
              selectedNoteId === note.id
                ? 'bg-white/[0.07] text-foreground'
                : 'text-muted-foreground hover:bg-white/[0.04] hover:text-foreground',
            )}
          >
            <span className="truncate flex-1 text-left">{note.title}</span>
            <span className="shrink-0 text-[10px] text-muted-foreground/60">
              {relativeTime(note.updated_at)}
            </span>
          </button>
        ))}
      </CollapsibleContent>
    </Collapsible>
  );
}

function ScopedSection({
  tree,
  totalCount,
  selectedNoteId,
  onSelectNote,
}: {
  tree: ReturnType<typeof buildScopeTree>;
  totalCount: number;
  selectedNoteId: string | null;
  onSelectNote: (note: NoteCompact) => void;
}) {
  const [open, setOpen] = useState(true);

  return (
    <Collapsible open={open} onOpenChange={setOpen}>
      <CollapsibleTrigger className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-xs font-medium transition-colors hover:bg-white/[0.04]">
        <HugeiconsIcon
          icon={ArrowDown01Icon}
          size={12}
          className={cn(
            'shrink-0 text-muted-foreground transition-transform duration-200',
            !open && '-rotate-90',
          )}
        />
        <span className="flex-1 text-left text-foreground">Scoped</span>
        <span className="text-[10px] text-muted-foreground/60">{totalCount}</span>
      </CollapsibleTrigger>
      <CollapsibleContent className="overflow-hidden">
        {tree.length === 0 ? (
          <p className="px-4 py-2 text-[11px] text-muted-foreground/60">No scoped notes</p>
        ) : (
          tree.map((node) => (
            <ScopeTreeNode
              key={node.fullPath}
              node={node}
              selectedNoteId={selectedNoteId}
              onSelectNote={onSelectNote}
              depth={1}
            />
          ))
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}

function SearchResultsList({
  results,
  selectedNoteId,
  onSelectNote,
}: {
  results: SearchResult[] | null;
  selectedNoteId: string | null;
  onSelectNote: (note: SearchResult) => void;
}) {
  if (!results) {
    return <p className="px-2 py-4 text-center text-xs text-muted-foreground">Searching...</p>;
  }

  if (results.length === 0) {
    return <p className="px-2 py-4 text-center text-xs text-muted-foreground">No results found</p>;
  }

  return (
    <div className="space-y-0.5">
      <p className="px-2 py-1 text-[11px] font-medium text-muted-foreground">
        Results ({results.length})
      </p>
      {results.map((result) => (
        <button
          key={result.id}
          type="button"
          onClick={() => onSelectNote(result)}
          className={cn(
            'flex w-full flex-col gap-0.5 rounded-md px-2 py-1.5 text-left text-xs transition-colors',
            selectedNoteId === result.id
              ? 'bg-white/[0.07] text-foreground'
              : 'text-muted-foreground hover:bg-white/[0.04] hover:text-foreground',
          )}
        >
          <div className="flex items-center gap-2">
            <Badge variant="secondary" className="h-4 px-1.5 text-[10px] shrink-0">
              {noteTypeLabel(result.note_type)}
            </Badge>
            <span className="truncate font-medium">{result.title}</span>
          </div>
          {result.snippet && (
            <p className="line-clamp-2 text-[11px] text-muted-foreground/70 pl-0.5">
              {result.snippet}
            </p>
          )}
        </button>
      ))}
    </div>
  );
}
