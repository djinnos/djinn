# Visual-spec: proposal and plan authoring conventions

You are authoring a living proposal document that will be read by humans and
machines.  The spec below governs how you write proposal markdown, how you
enrich it toward MDX as the plan hardens, and how you treat the memory layer
for learned refinements.

## Progressive markdown-to-MDX enrichment

Start every proposal in plain markdown.  As the plan stabilises and structural
precision becomes load-bearing, promote blocks to MDX incrementally:

1. **Draft stage** — plain prose, bullet lists, and fenced code blocks.
2. **Structural stage** — replace prose scaffolding with MDX components
   (`<Callout>`, `<StepList>`, `<DependencyGraph>`) when the content is stable
   enough that a malformed component would be worse than prose.
3. **Spec stage** — every block that downstream tooling consumes (acceptance
   criteria, task breakdowns, dependency edges) should be a first-class MDX
   block with typed props.

Never skip stages.  Premature MDX is worse than late MDX because it freezes
structure before the content has settled.

## Block authoring quality

Every block in a proposal must be:

- **Self-contained** — a reader can understand the block without scrolling to
  a different section.
- **Attributable** — every claim links to evidence (a code path, a memory note,
  a test, or a verified observation).
- **Testable** — acceptance criteria blocks describe observable outcomes, not
  intentions.

Avoid hollow blocks that exist only for layout.  If a section cannot state
a concrete claim or criterion, merge it into its parent.

## Bare `<` / `>` backtick constraint

When you write literal angle brackets in markdown — for example `<div>`,
`Option<T>`, or template syntax — you **must** wrap them in backtick-fenced
code spans (`\`<div>\``, `\`Option<T>\``).  Bare `<` and `>` characters
outside code fences will be interpreted as HTML/JSX by MDX processors and
silently corrupt the rendered output.

This constraint applies everywhere in proposal markdown: prose, lists, code
comments inside fenced blocks, and table cells.  The only exception is actual
HTML or JSX that you intend to be rendered.

## Memory as the editable learned/refinement layer

Memory notes are the **learned and refined** layer of knowledge.  They are
editable by the team and should not be confused with the immutable native
skill content.

- When you discover a refinement (a correction, a pitfall, a design pattern),
  write or update a memory note — do not patch the native skill body.
- When the planner or reviewer surfaces a new constraint, capture it in
  memory and link it from the proposal with `[[wikilinks]]`.
- Native skill content is versioned and compiled into the platform; it is
  not the right surface for per-project or per-session learnings.

Think of native skills as the **constitution** and memory as the **case law**.
The constitution changes rarely and through deliberate process; case law
evolves continuously from lived experience.
