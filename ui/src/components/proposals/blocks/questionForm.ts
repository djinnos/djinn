// Pure (React-free) helpers for the "Open Questions" block. djinn stores the
// outstanding questions as the block's *children text* — the LLM hand-writes
// them as prose/bullets, NOT as a structured `questions={…}` prop (the schema's
// `questions[]` field exists only to guide generation; see the Rust registry in
// `proposal_blocks.rs`). So this parser folds that freeform markdown into a flat
// list of questions, each with optional sub-detail lines, for a nicer read-only
// render. Kept in its own module so it is unit-testable without pulling React in.
//
// This block is intentionally a READ-ONLY list — NOT a form. There is no answer
// capture, no inputs, no submit. We only render the outstanding questions more
// nicely than a plain-markdown blob.
//
// A new question starts on a line that is one of:
//   1. a numbered line — `1.` / `1)` / `(1)`
//   2. a `###`(+) markdown heading
//   3. a **bolded** line — the whole (trimmed) line wrapped in `**…**`
//   4. a line that ends in `?` (a question sentence)
//   5. a TOP-LEVEL `-`/`*`/`+` bullet that ends in `?` — the most common
//      authoring pattern is a flat bulleted list of questions, so each base-
//      indentation question-bullet is its own question.
// Following plain lines, or bullets that are INDENTED under the current
// question / do not end in `?`, become that question's detail. Anything before
// the first recognised question header is attached to the first question as
// leading detail (or, if there is no header at all, the whole body becomes a
// single question). Empty input yields `[]` so the caller can fall back to the
// raw markdown render.

/** One parsed outstanding question: its text plus optional muted sub-detail. */
export interface ParsedQuestion {
  /** The question text, markdown preserved for inline rendering. */
  question: string;
  /** Sub-points / clarifying detail lines shown as muted secondary text. */
  detail: string[];
  /** True when the author tagged the item "(recommended)". */
  recommended: boolean;
}

/** Strip a trailing/inline `(recommended)` tag, returning the cleaned text. */
function extractRecommended(text: string): { text: string; recommended: boolean } {
  // Match `(recommended)` case-insensitively, anywhere, optionally bracketed.
  const re = /\s*[([]?\s*recommended\s*[)\]]?/i;
  if (re.test(text)) {
    return { text: text.replace(re, "").trim(), recommended: true };
  }
  return { text: text.trim(), recommended: false };
}

/** A leading numbered marker: `1.`, `1)`, `(1)` — captured + stripped. */
const NUMBERED = /^\s*\(?(\d{1,3})[.)]\s+(.*)$/;
/** A markdown heading line: `#`..`######` followed by text. */
const HEADING = /^\s*#{1,6}\s+(.*)$/;
/** A bullet line: `-` / `*` / `+` followed by text. */
const BULLET = /^\s*[-*+]\s+(.*)$/;

/** Leading-whitespace indent of a line, counting a tab as 2 spaces (file-tree
 * parser convention) so a top-level `- q?` is distinguishable from a nested
 * `  - detail?`. */
function indentOf(line: string): number {
  const ws = /^[ \t]*/.exec(line)?.[0] ?? "";
  let n = 0;
  for (const ch of ws) n += ch === "\t" ? 2 : 1;
  return n;
}

/** True when a (trimmed) line ends in `?`, ignoring trailing markdown emphasis
 * (e.g. `…?**` or `…?*`). */
function endsInQuestion(line: string): boolean {
  return /\?\s*\**\s*$/.test(line.trim());
}

/** True when a (trimmed) line is entirely wrapped in `**…**` (bold). */
function isBoldLine(line: string): boolean {
  const t = line.trim();
  return /^\*\*[\s\S]+\*\*[.?!:]?$/.test(t) && t.length > 4;
}

/** Unwrap a fully-bold line's inner text (keeping a trailing `?`/punctuation). */
function unwrapBold(line: string): string {
  const t = line.trim();
  const m = /^\*\*([\s\S]+?)\*\*([.?!:]?)$/.exec(t);
  return m ? `${m[1]}${m[2] ?? ""}`.trim() : t;
}

/**
 * Decide whether a non-empty line begins a NEW question. Returns the question
 * text (markers stripped) when it does, otherwise `null` (it's detail).
 */
function asQuestionHeader(line: string): string | null {
  const numbered = NUMBERED.exec(line);
  if (numbered) return (numbered[2] ?? "").trim();

  const heading = HEADING.exec(line);
  if (heading) return (heading[1] ?? "").trim();

  if (isBoldLine(line)) return unwrapBold(line);

  // A non-bullet sentence that ends in `?` (ignoring trailing markdown emphasis)
  // is a question header. Bullets are handled separately in the parse loop,
  // where indentation is available to tell a top-level question-bullet from a
  // nested detail bullet.
  if (!BULLET.test(line) && endsInQuestion(line)) {
    return line.trim();
  }

  return null;
}

/**
 * The base indentation of the bulleted list — the minimum indent among all
 * bullet lines — so a list authored at column 0 OR uniformly indented both read
 * their top-level bullets as questions. Returns `null` when there are no
 * bullets.
 */
function baseBulletIndent(lines: string[]): number | null {
  let base: number | null = null;
  for (const line of lines) {
    if (!line.trim()) continue;
    if (BULLET.test(line)) {
      const indent = indentOf(line);
      if (base === null || indent < base) base = indent;
    }
  }
  return base;
}

/**
 * Parse the freeform <QuestionForm> body into a flat list of outstanding
 * questions, each with optional sub-detail. Returns `[]` for empty/whitespace
 * input so the caller can fall back to the raw markdown render. Never throws.
 */
export function parseQuestions(body: string): ParsedQuestion[] {
  if (!body || !body.trim()) return [];
  const lines = body.replace(/\r\n?/g, "\n").split("\n");

  const questions: ParsedQuestion[] = [];
  // Detail seen before any recognised header — attached to the first question.
  const leading: string[] = [];

  // The base (minimum) indent of the bulleted list, so a top-level question-
  // bullet is distinguishable from a nested detail bullet. `null` = no bullets.
  const base = baseBulletIndent(lines);
  // Whether the CURRENT question was opened by a top-level question-bullet. Only
  // in this "bullet-list" mode does a following base-indent `?`-bullet start a
  // new question; bullets under a numbered/heading/bold question stay its detail.
  let inBulletList = false;

  const pushDetail = (raw: string) => {
    const bullet = BULLET.exec(raw);
    const text = (bullet ? bullet[1] : raw).trim();
    if (!text) return;
    if (questions.length === 0) {
      leading.push(text);
    } else {
      questions[questions.length - 1]!.detail.push(text);
    }
  };

  const pushQuestion = (header: string, fromBullet: boolean): void => {
    const { text, recommended } = extractRecommended(header);
    // A header that becomes empty after stripping `(recommended)` is just a
    // stray tag — treat it as detail rather than an empty question.
    if (!text) {
      if (recommended && questions.length > 0) {
        questions[questions.length - 1]!.recommended = true;
      }
      return;
    }
    questions.push({ question: text, detail: [], recommended });
    inBulletList = fromBullet;
  };

  for (const raw of lines) {
    if (!raw.trim()) continue;

    const bullet = BULLET.exec(raw);
    if (bullet) {
      // A top-level bullet that is a question is its own question — but only
      // when it sits at the list's base indent AND we are not collecting
      // detail bullets under a non-bullet (numbered/heading/bold) question.
      const topLevel = base !== null && indentOf(raw) <= base;
      const startsQuestion =
        topLevel && endsInQuestion(raw) && (questions.length === 0 || inBulletList);
      if (startsQuestion) {
        pushQuestion((bullet[1] ?? "").trim(), true);
      } else {
        pushDetail(raw);
      }
      continue;
    }

    const header = asQuestionHeader(raw);
    if (header !== null && header !== "") {
      pushQuestion(header, false);
    } else {
      pushDetail(raw);
    }
  }

  // No header was ever recognised: treat the whole body as one question so the
  // block still renders as a single clean card instead of falling back to raw.
  if (questions.length === 0) {
    const all = lines.map((l) => l.trim()).filter(Boolean);
    if (all.length === 0) return [];
    const first = all[0]!;
    const { text, recommended } = extractRecommended(first);
    return [
      {
        question: text || first,
        detail: all.slice(1),
        recommended,
      },
    ];
  }

  // Attach any leading pre-header detail to the first question.
  if (leading.length > 0) {
    questions[0]!.detail.unshift(...leading);
  }

  return questions;
}
