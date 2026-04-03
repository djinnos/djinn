import { useMemo } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { Badge } from '@/components/ui/badge';
import { HugeiconsIcon } from '@hugeicons/react';
import { Brain01Icon, Loading02Icon } from '@hugeicons/core-free-icons';
import { cn } from '@/lib/utils';
import { noteTypeLabel, parseScopePaths, relativeTime } from './memoryUtils';
import type { MemoryReadOutput } from '@/api/generated/mcp-tools.gen';

const WIKILINK_RE = /\[\[([^\]]+)\]\]/g;

/** Marker prefix injected into href so we can detect wikilinks in the component override. */
const WIKILINK_PREFIX = '#wikilink:';

/** Replace [[Title]] with markdown links using a fragment-only href (no navigation). */
function expandWikilinks(md: string): string {
  return md.replace(WIKILINK_RE, (_match, title: string) => {
    return `[${title}](${WIKILINK_PREFIX}${encodeURIComponent(title)})`;
  });
}

interface MemoryNoteDetailProps {
  note: MemoryReadOutput | null;
  loading: boolean;
  onNavigateToNote?: (title: string) => void;
}

export function MemoryNoteDetail({ note, loading, onNavigateToNote }: MemoryNoteDetailProps) {
  if (loading) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <HugeiconsIcon icon={Loading02Icon} size={24} className="animate-spin text-muted-foreground" />
      </div>
    );
  }

  if (!note) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 text-muted-foreground">
        <HugeiconsIcon icon={Brain01Icon} size={32} className="opacity-40" />
        <p className="text-sm">Select a note to view its content</p>
      </div>
    );
  }

  const scopePaths = parseScopePaths(
    // scope_paths comes through as string[] from memory_read or as raw JSON
    typeof note.scope_paths === 'string'
      ? note.scope_paths
      : JSON.stringify(note.scope_paths ?? []),
  );
  const tags: string[] = Array.isArray(note.tags) ? note.tags : [];
  const confidence = typeof note.confidence === 'number' ? note.confidence : null;

  return (
    <div className="flex flex-1 flex-col min-h-0">
      {/* Header */}
      <div className="shrink-0 border-b border-border px-6 py-4 space-y-3">
        <div className="flex items-start gap-3">
          <h1 className="flex-1 text-lg font-semibold text-foreground leading-tight">
            {note.title}
          </h1>
          {note.note_type && (
            <Badge variant="secondary" className="shrink-0">
              {noteTypeLabel(note.note_type)}
            </Badge>
          )}
        </div>

        {/* Metadata */}
        <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
          {scopePaths.length > 0 && (
            <div className="flex flex-wrap items-center gap-1">
              {scopePaths.map((p) => (
                <span
                  key={p}
                  className="inline-flex items-center rounded-md bg-white/[0.05] px-1.5 py-0.5 text-[11px] font-mono text-muted-foreground"
                >
                  {p}
                </span>
              ))}
            </div>
          )}
          {tags.length > 0 && (
            <div className="flex flex-wrap items-center gap-1">
              {tags.map((tag) => (
                <Badge key={tag} variant="outline" className="h-4 px-1.5 text-[10px]">
                  {tag}
                </Badge>
              ))}
            </div>
          )}
          {confidence !== null && (
            <ConfidenceMeter value={confidence} />
          )}
          {note.updated_at && (
            <span className="text-muted-foreground/60">
              updated {relativeTime(note.updated_at)}
            </span>
          )}
        </div>
      </div>

      {/* Content */}
      <NoteContent
        content={note.content ?? ''}
        onNavigateToNote={onNavigateToNote}
      />
    </div>
  );
}

function NoteContent({
  content,
  onNavigateToNote,
}: {
  content: string;
  onNavigateToNote?: (title: string) => void;
}) {
  const expanded = useMemo(() => expandWikilinks(content), [content]);

  const components = useMemo(
    () => ({
      a: ({ href, children }: React.ComponentProps<'a'>) => {
        if (href?.startsWith(WIKILINK_PREFIX)) {
          const title = decodeURIComponent(href.slice(WIKILINK_PREFIX.length));
          return (
            <span
              role="link"
              tabIndex={0}
              onClick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                onNavigateToNote?.(title);
              }}
              onKeyDown={(e) => {
                if (e.key === 'Enter') onNavigateToNote?.(title);
              }}
              className="text-blue-400 hover:text-blue-300 cursor-pointer hover:underline"
            >
              {children}
            </span>
          );
        }
        return <a href={href}>{children}</a>;
      },
    }),
    [onNavigateToNote],
  );

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto max-w-3xl px-6 py-6">
        <div className="prose prose-sm max-w-none dark:prose-invert">
          <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
            {expanded}
          </ReactMarkdown>
        </div>
      </div>
    </div>
  );
}

function ConfidenceMeter({ value }: { value: number }) {
  const pct = Math.round(value * 100);
  const color =
    value >= 0.7
      ? 'bg-emerald-400'
      : value >= 0.4
        ? 'bg-amber-400'
        : 'bg-red-400';

  return (
    <div className="flex items-center gap-1.5" title={`Confidence: ${pct}%`}>
      <div className="h-1.5 w-12 rounded-full bg-white/[0.08] overflow-hidden">
        <div
          className={cn('h-full rounded-full transition-all', color)}
          style={{ width: `${pct}%` }}
        />
      </div>
      <span className="text-[10px] text-muted-foreground/60">{pct}%</span>
    </div>
  );
}
