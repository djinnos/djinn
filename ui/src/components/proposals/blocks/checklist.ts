// Pure (React-free) helpers for the Checklist block. djinn stores the checklist
// as the block's *children text* — GitHub task-list lines (`- [x] done` /
// `- [ ] todo`) and/or plain bullets, each with an optional trailing note after
// an em/en-dash or a double-space. This module parses those lines into items
// reflecting the AUTHORED checked state. Kept React-free so it is unit-testable.
//
// Ported from agent-native's `checklist` block. djinn is READ-ONLY: the
// authored `[x]`/`[ ]` state is rendered as-is and is NOT toggleable by viewers
// (no submit / state capture), matching djinn's no-form product stance.
//
// Robustness: if no list items parse, `parseChecklist` returns an empty array
// so the component can fall back to `BlockMarkdown`. The parser never throws.

/** A single parsed checklist item with its authored checked state. */
export interface ChecklistItem {
  /** Whether the author marked the item done (`- [x]`). Plain bullets are false. */
  checked: boolean;
  /** The item text (without the bullet / checkbox marker / trailing note). */
  label: string;
  /** Optional muted note authored after an em/en-dash or double space. */
  note?: string;
}

/** A parsed checklist plus its done/total tally. */
export interface ParsedChecklist {
  items: ChecklistItem[];
  done: number;
  total: number;
}

// `- [x] label`, `* [ ] label`, `+ [X] label`, or a plain `- label` bullet.
const ITEM_RE = /^[-*+]\s+(?:\[([ xX])\]\s+)?(.*)$/;

// A trailing note separated from the label by ` — `, ` – `, ` -- `, or `  `.
const NOTE_RE = /^(.*?)(?:\s+[—–]\s+|\s+--\s+|\s{2,})(.+)$/;

/**
 * Parse the children text into checklist items. Each non-empty line that looks
 * like a bullet (`-`/`*`/`+`) becomes an item; a `[x]`/`[X]` marker sets
 * `checked`. A trailing note is split off the label. Lines that aren't bullets
 * are ignored. Returns `{ items: [], done: 0, total: 0 }` when nothing parses.
 */
export function parseChecklist(body: string): ParsedChecklist {
  const items: ChecklistItem[] = [];

  for (const rawLine of body.split("\n")) {
    const line = rawLine.trim();
    if (line.length === 0) continue;

    const match = ITEM_RE.exec(line);
    if (!match) continue;

    const checked = match[1]?.toLowerCase() === "x";
    const rest = match[2].trim();
    if (rest.length === 0) continue;

    const noteMatch = NOTE_RE.exec(rest);
    if (noteMatch) {
      items.push({
        checked,
        label: noteMatch[1].trim(),
        note: noteMatch[2].trim(),
      });
    } else {
      items.push({ checked, label: rest });
    }
  }

  const done = items.filter((item) => item.checked).length;
  return { items, done, total: items.length };
}
