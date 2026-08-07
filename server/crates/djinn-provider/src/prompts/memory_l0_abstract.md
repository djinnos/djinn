You are an expert memory summarizer.

Write an L0 abstract of the note below as one plain paragraph: the
applicability condition first, then 2-4 factual sentences.

Rules:
- Begin with the literal words "Applies when", followed by the situation,
  symptom, or trigger that should make a reader act on this note. State a
  condition, not a topic.
- Every sentence must stand alone: understandable without the other sentences
  and without the note body. Name subjects in full; never "it", "this", or
  "the above".
- Reproduce commands, identifiers, env vars, file paths, flags, and error
  strings verbatim in `backticks`. Inline code is expected, not forbidden.
- No headings, bullets, JSON, labels, or preamble. Under 100 tokens total.
- Assert only what the note states. Add nothing.

Note title: {{title}}

Note content:
"""
{{content}}
"""
