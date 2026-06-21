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
// Following plain lines or `-`/`*`/`+` bullets become that question's detail.
// Anything before the first recognised question header is attached to the first
// question as leading detail (or, if there is no header at all, the whole body
// becomes a single question). Empty input yields `[]` so the caller can fall
// back to the raw markdown render.

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

  // A sentence that ends in `?` (ignoring trailing markdown emphasis) is a
  // question header — but a bullet/detail line that happens to end in `?` should
  // stay detail, so bullets are excluded here (handled as detail below).
  if (!BULLET.test(line) && /\?\s*\**\s*$/.test(line.trim())) {
    return line.trim();
  }

  return null;
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

  for (const raw of lines) {
    if (!raw.trim()) continue;

    const header = asQuestionHeader(raw);
    if (header !== null && header !== "") {
      const { text, recommended } = extractRecommended(header);
      // A header that becomes empty after stripping `(recommended)` is just a
      // stray tag — treat it as detail rather than an empty question.
      if (!text) {
        if (recommended && questions.length > 0) {
          questions[questions.length - 1]!.recommended = true;
        }
        continue;
      }
      questions.push({ question: text, detail: [], recommended });
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
