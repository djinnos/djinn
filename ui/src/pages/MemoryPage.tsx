import { useState, useEffect, useCallback, useRef } from 'react';
import { callMcpTool } from '@/api/mcpClient';
import {
  useSelectedProject,
  useIsAllProjects,
} from '@/stores/useProjectStore';
import { MemoryExplorer } from '@/components/memory/MemoryExplorer';
import { MemoryNoteDetail } from '@/components/memory/MemoryNoteDetail';
import { MemoryGraphCanvas } from '@/components/memory/MemoryGraphCanvas';
import { HugeiconsIcon } from '@hugeicons/react';
import { Brain01Icon, ConnectIcon } from '@hugeicons/core-free-icons';
import { cn } from '@/lib/utils';
import type {
  MemoryListOutputSchema,
  MemorySearchOutputSchema,
  MemoryReadOutput,
} from '@/api/generated/mcp-tools.gen';

type NoteCompact = MemoryListOutputSchema.NoteCompact;
type SearchResult = MemorySearchOutputSchema.MemorySearchResultItem;

export function MemoryPage() {
  const project = useSelectedProject();
  const isAll = useIsAllProjects();

  const [notes, setNotes] = useState<NoteCompact[]>([]);
  const [selectedNote, setSelectedNote] = useState<MemoryReadOutput | null>(null);
  const [selectedNoteId, setSelectedNoteId] = useState<string | null>(null);
  const [searchQuery, setSearchQuery] = useState('');
  const [searchResults, setSearchResults] = useState<SearchResult[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [detailLoading, setDetailLoading] = useState(false);
  const [listError, setListError] = useState<string | null>(null);
  const [view, setView] = useState<'list' | 'graph'>('list');

  const noteCache = useRef(new Map<string, MemoryReadOutput>());
  const searchTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const projectSlug = project
    ? `${project.github_owner}/${project.github_repo}`
    : undefined;

  // Fetch the compact notes list on project change
  const refresh = useCallback(() => {
    if (!projectSlug) return;
    setLoading(true);
    setListError(null);
    setSelectedNote(null);
    setSelectedNoteId(null);
    setSearchQuery('');
    setSearchResults(null);
    noteCache.current.clear();

    callMcpTool('memory_list', { project: projectSlug, depth: 0 })
      .then((listResult) => {
        setNotes(listResult.notes ?? []);
      })
      .catch((err: unknown) => {
        setNotes([]);
        setListError(err instanceof Error ? err.message : 'Failed to load memory notes');
      })
      .finally(() => setLoading(false));
  }, [projectSlug]);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect -- triggers the async notes load on project change; the synchronous reset is the fetch transition, not derivable state.
    refresh();
  }, [refresh]);

  // Select a note and fetch full content
  const handleSelectNote = useCallback(
    (note: NoteCompact | SearchResult) => {
      if (!projectSlug) return;
      setSelectedNoteId(note.id);

      const cached = noteCache.current.get(note.permalink);
      if (cached) {
        setSelectedNote(cached);
        return;
      }

      setDetailLoading(true);
      callMcpTool('memory_read', { project: projectSlug, identifier: note.permalink })
        .then((result) => {
          noteCache.current.set(note.permalink, result);
          setSelectedNote(result);
        })
        .catch(() => setSelectedNote(null))
        .finally(() => setDetailLoading(false));
    },
    [projectSlug],
  );

  // Navigate to a note by title (from wikilinks)
  const handleNavigateToNote = useCallback(
    (title: string) => {
      if (!projectSlug) return;

      // Try to find in the already-loaded list by title match
      const match = notes.find(
        (n) => n.title.toLowerCase() === title.toLowerCase(),
      );
      if (match) {
        handleSelectNote(match);
        return;
      }

      // Fall back to memory_read by title (the server resolves title → permalink)
      setDetailLoading(true);
      callMcpTool('memory_read', { project: projectSlug, identifier: title })
        .then((result) => {
          if (result.id) {
            setSelectedNoteId(result.id);
            setSelectedNote(result);
            if (result.permalink) {
              noteCache.current.set(result.permalink, result);
            }
          }
        })
        .catch(() => setSelectedNote(null))
        .finally(() => setDetailLoading(false));
    },
    [projectSlug, notes, handleSelectNote],
  );

  // Open a note by permalink — called from the MemoryGraphCanvas node-click.
  // Switches to the list view and fetches the note via the existing detail flow.
  const handleSelectNoteByPermalink = useCallback(
    (permalink: string) => {
      if (!projectSlug) return;
      setView('list');

      // Try to find in the already-loaded list by permalink match.
      const match = notes.find((n) => n.permalink === permalink);
      if (match) {
        handleSelectNote(match);
        return;
      }

      // Fall back to memory_read by permalink.
      setDetailLoading(true);
      callMcpTool('memory_read', { project: projectSlug, identifier: permalink })
        .then((result) => {
          if (result.id) {
            setSelectedNoteId(result.id);
            setSelectedNote(result);
            if (result.permalink) {
              noteCache.current.set(result.permalink, result);
            }
          }
        })
        .catch(() => setSelectedNote(null))
        .finally(() => setDetailLoading(false));
    },
    [projectSlug, notes, handleSelectNote],
  );

  // Debounced search
  const handleSearchChange = useCallback(
    (query: string) => {
      setSearchQuery(query);

      if (searchTimer.current) clearTimeout(searchTimer.current);

      if (!query.trim()) {
        setSearchResults(null);
        return;
      }

      if (!projectSlug) return;

      setSearchResults(null); // show loading state
      searchTimer.current = setTimeout(() => {
        callMcpTool('memory_search', { project: projectSlug, query: query.trim() })
          .then((result) => setSearchResults(result.results ?? []))
          .catch(() => setSearchResults([]));
      }, 200);
    },
    [projectSlug],
  );

  const showEmpty = isAll || !project;

  return (
    <div className="flex h-full min-h-0 flex-col">
      {showEmpty ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 text-muted-foreground">
          <HugeiconsIcon icon={Brain01Icon} size={32} className="opacity-40" />
          <p className="text-sm">Select a project to view its knowledge base</p>
        </div>
      ) : loading ? (
        <div className="flex flex-1 items-center justify-center">
          <div className="h-5 w-5 animate-spin rounded-full border-2 border-muted-foreground border-t-transparent" />
        </div>
      ) : listError ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 text-muted-foreground">
          <HugeiconsIcon icon={Brain01Icon} size={32} className="opacity-40" />
          <p className="text-sm">Failed to load memory notes</p>
          <p className="max-w-md text-center text-xs text-muted-foreground/70">{listError}</p>
          <button
            type="button"
            onClick={refresh}
            className="rounded-md border border-border px-3 py-1.5 text-xs transition-colors hover:bg-white/[0.04]"
          >
            Retry
          </button>
        </div>
      ) : (
        <>
          <ViewToggle view={view} onChange={setView} />
          {view === 'graph' ? (
            <div className="min-h-0 flex-1">
              <MemoryGraphCanvas
                projectSlug={projectSlug!}
                onSelectNote={handleSelectNoteByPermalink}
              />
            </div>
          ) : (
            <div className="flex min-h-0 flex-1">
              <MemoryExplorer
                notes={notes}
                searchQuery={searchQuery}
                onSearchChange={handleSearchChange}
                searchResults={searchResults}
                selectedNoteId={selectedNoteId}
                onSelectNote={handleSelectNote}
              />
              <MemoryNoteDetail
                note={selectedNote}
                loading={detailLoading}
                onNavigateToNote={handleNavigateToNote}
              />
            </div>
          )}
        </>
      )}
    </div>
  );
}

function ViewToggle({
  view,
  onChange,
}: {
  view: 'list' | 'graph';
  onChange: (view: 'list' | 'graph') => void;
}) {
  return (
    <div className="flex shrink-0 items-center gap-1 border-b border-border/60 bg-background/40 px-4 py-1.5">
      <ToggleChip
        active={view === 'list'}
        onClick={() => onChange('list')}
      >
        List
      </ToggleChip>
      <ToggleChip
        active={view === 'graph'}
        onClick={() => onChange('graph')}
      >
        <HugeiconsIcon icon={ConnectIcon} size={12} className="shrink-0" />
        Graph
      </ToggleChip>
    </div>
  );
}

function ToggleChip({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        'flex items-center gap-1 rounded-md px-2 py-1 text-xs transition-colors',
        active
          ? 'bg-white/[0.07] text-foreground'
          : 'text-muted-foreground hover:bg-white/[0.04] hover:text-foreground',
      )}
    >
      {children}
    </button>
  );
}
