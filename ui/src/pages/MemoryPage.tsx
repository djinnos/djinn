import { useState, useEffect, useCallback, useRef } from 'react';
import { callMcpTool } from '@/api/mcpClient';
import { useSelectedProject, useIsAllProjects } from '@/stores/useProjectStore';
import { MemoryExplorer } from '@/components/memory/MemoryExplorer';
import { MemoryNoteDetail } from '@/components/memory/MemoryNoteDetail';
import { HugeiconsIcon } from '@hugeicons/react';
import { Brain01Icon } from '@hugeicons/core-free-icons';
import type {
  MemoryListOutputSchema,
  MemorySearchOutputSchema,
  MemoryReadOutput,
  MemoryHealthOutput,
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
  const [health, setHealth] = useState<MemoryHealthOutput | null>(null);

  const noteCache = useRef(new Map<string, MemoryReadOutput>());
  const searchTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const projectSlug = project
    ? `${project.github_owner}/${project.github_repo}`
    : undefined;

  // Fetch notes list + health on project change
  const refresh = useCallback(() => {
    if (!projectSlug) return;
    setLoading(true);
    setSelectedNote(null);
    setSelectedNoteId(null);
    setSearchQuery('');
    setSearchResults(null);
    noteCache.current.clear();

    Promise.all([
      callMcpTool('memory_list', { project: projectSlug, depth: 0 }),
      callMcpTool('memory_health', { project: projectSlug }),
    ])
      .then(([listResult, healthResult]) => {
        setNotes(listResult.notes ?? []);
        setHealth(healthResult);
      })
      .catch(() => {
        setNotes([]);
        setHealth(null);
      })
      .finally(() => setLoading(false));
  }, [projectSlug]);

  useEffect(() => {
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

  if (isAll || !project) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 text-muted-foreground">
        <HugeiconsIcon icon={Brain01Icon} size={32} className="opacity-40" />
        <p className="text-sm">Select a project to view its knowledge base</p>
      </div>
    );
  }

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center">
        <div className="h-5 w-5 animate-spin rounded-full border-2 border-muted-foreground border-t-transparent" />
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1">
      <MemoryExplorer
        notes={notes}
        searchQuery={searchQuery}
        onSearchChange={handleSearchChange}
        searchResults={searchResults}
        selectedNoteId={selectedNoteId}
        onSelectNote={handleSelectNote}
        health={health}
      />
      <MemoryNoteDetail
        note={selectedNote}
        loading={detailLoading}
        onNavigateToNote={handleNavigateToNote}
      />
    </div>
  );
}
