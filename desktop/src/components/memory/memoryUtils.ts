import type { MemoryListOutputSchema } from '@/api/generated/mcp-tools.gen';

type NoteCompact = MemoryListOutputSchema.NoteCompact;

export const NOTE_TYPE_LABELS: Record<string, string> = {
  adr: 'Decisions',
  pattern: 'Patterns',
  pitfall: 'Pitfalls',
  case: 'Cases',
  research: 'Research',
  requirement: 'Requirements',
  reference: 'References',
  design: 'Design',
  session: 'Sessions',
  persona: 'Personas',
  journey: 'Journeys',
  design_spec: 'Design Specs',
  competitive: 'Competitive',
  tech_spike: 'Tech Spikes',
  brief: 'Brief',
  roadmap: 'Roadmap',
};

/** Note types hidden from the UI. */
export const HIDDEN_NOTE_TYPES = new Set(['repo_map']);

export const NOTE_TYPE_ORDER: string[] = [
  'adr',
  'pattern',
  'pitfall',
  'case',
  'requirement',
  'design',
  'design_spec',
  'persona',
  'journey',
  'research',
  'tech_spike',
  'competitive',
  'session',
  'reference',
  'brief',
  'roadmap',
];

export function parseScopePaths(raw: string | undefined | null): string[] {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

export function partitionNotes(notes: NoteCompact[]): {
  global: NoteCompact[];
  scoped: NoteCompact[];
} {
  const global: NoteCompact[] = [];
  const scoped: NoteCompact[] = [];
  for (const note of notes) {
    if (HIDDEN_NOTE_TYPES.has(note.note_type)) continue;
    const paths = parseScopePaths(note.scope_paths);
    if (paths.length === 0) {
      global.push(note);
    } else {
      scoped.push(note);
    }
  }
  return { global, scoped };
}

export function groupByType(notes: NoteCompact[]): Map<string, NoteCompact[]> {
  const map = new Map<string, NoteCompact[]>();
  for (const note of notes) {
    const existing = map.get(note.note_type);
    if (existing) {
      existing.push(note);
    } else {
      map.set(note.note_type, [note]);
    }
  }
  return map;
}

export function sortedTypeEntries(
  grouped: Map<string, NoteCompact[]>,
): [string, NoteCompact[]][] {
  const entries = [...grouped.entries()];
  entries.sort((a, b) => {
    const ai = NOTE_TYPE_ORDER.indexOf(a[0]);
    const bi = NOTE_TYPE_ORDER.indexOf(b[0]);
    return (ai === -1 ? 999 : ai) - (bi === -1 ? 999 : bi);
  });
  return entries;
}

// --- Scope tree ---

export interface ScopeTreeNode {
  /** Display label — may be compressed like "server/crates/djinn-db" */
  segment: string;
  /** Full path from root */
  fullPath: string;
  children: ScopeTreeNode[];
  notes: NoteCompact[];
}

export function buildScopeTree(notes: NoteCompact[]): ScopeTreeNode[] {
  const root: ScopeTreeNode[] = [];

  for (const note of notes) {
    const scopePaths = parseScopePaths(note.scope_paths);
    for (const scopePath of scopePaths) {
      const segments = scopePath.split('/').filter(Boolean);
      insertIntoTree(root, segments, '', note);
    }
  }

  compressTree(root);
  sortTree(root);
  return root;
}

function insertIntoTree(
  children: ScopeTreeNode[],
  segments: string[],
  parentPath: string,
  note: NoteCompact,
): void {
  if (segments.length === 0) return;

  const [head, ...rest] = segments;
  const fullPath = parentPath ? `${parentPath}/${head}` : head;

  let node = children.find((c) => c.fullPath === fullPath);
  if (!node) {
    node = { segment: head, fullPath, children: [], notes: [] };
    children.push(node);
  }

  if (rest.length === 0) {
    node.notes.push(note);
  } else {
    insertIntoTree(node.children, rest, fullPath, note);
  }
}

/** Compress single-child intermediate directories: a/b/c → "a/b/c" */
function compressTree(nodes: ScopeTreeNode[]): void {
  for (const node of nodes) {
    while (node.children.length === 1 && node.notes.length === 0) {
      const child = node.children[0];
      node.segment = `${node.segment}/${child.segment}`;
      node.fullPath = child.fullPath;
      node.children = child.children;
      node.notes = child.notes;
    }
    compressTree(node.children);
  }
}

function sortTree(nodes: ScopeTreeNode[]): void {
  nodes.sort((a, b) => a.segment.localeCompare(b.segment));
  for (const node of nodes) {
    sortTree(node.children);
  }
}

/** Count total notes in a tree node (including descendants). */
export function countTreeNotes(node: ScopeTreeNode): number {
  let count = node.notes.length;
  for (const child of node.children) {
    count += countTreeNotes(child);
  }
  return count;
}

/** Format a relative timestamp like "2d ago", "5h ago", etc. */
export function relativeTime(iso: string): string {
  const now = Date.now();
  const then = new Date(iso).getTime();
  const diff = now - then;
  if (diff < 0) return 'just now';

  const minutes = Math.floor(diff / 60_000);
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes}m ago`;

  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;

  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;

  const months = Math.floor(days / 30);
  return `${months}mo ago`;
}

export function noteTypeLabel(type: string): string {
  return NOTE_TYPE_LABELS[type] ?? type;
}
