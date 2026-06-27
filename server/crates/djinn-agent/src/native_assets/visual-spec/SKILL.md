# Visual-spec: proposal and plan authoring conventions

You are authoring a living proposal document that will be read by humans and
machines.  The spec below governs how you write proposal markdown, how you
enrich it toward MDX as the plan hardens, and how you treat the memory layer
for learned refinements.

## Use the right block for each kind of content

Proposals are reviewed by humans. A wall of prose and plain code fences is hard
to scan. A spec headed for graduation should be **richly visual**: reach for the
MDX block that matches the content instead of leaving it as prose or a markdown
fence. Pull the available vocabulary with `get_block_catalog` and map content to
blocks:

| Content | Use this block | Instead of |
| --- | --- | --- |
| A file / directory layout | `<FileTree>` | a ```text fence |
| Code the proposal references | `<AnnotatedCode>` | a bare code fence |
| Architecture, data flow, sequence, state | `<Diagram>` (mermaid) | prose describing the flow |
| A request/response or API shape | `<ApiEndpoint>` | prose |
| A decision with options + trade-offs | `<Decisions>` | a prose paragraph |
| A key warning, note, or rationale | `<Callout>` | a bolded sentence |
| Acceptance criteria / steps / tasks | `<Checklist>` | a `- [ ]` list |
| A side-by-side comparison (before/after, A vs B) | `<Columns>` | stacked prose |
| A before/after change | `<Diff>` | two code fences |
| A UI mockup / screen layout | `<Wireframe>` | a prose description |
| A large JSON example | `<JsonExplorer>` | a ```json fence |
| Open questions for the team | `<QuestionForm>` | a bullet list |

Rules of thumb:

- A **file map is ALWAYS a `<FileTree>`**, never a text fence.
- **Code the proposal references is ALWAYS `<AnnotatedCode>`**, never a bare fence.
- Every proposal of any complexity should carry at least one `<Diagram>` of its
  core flow or architecture.
- **Never invent block tags.** Use only the registered vocabulary returned by
  `get_block_catalog` — an unknown tag (e.g. a made-up `<ReadinessRemediation>`)
  is rejected.

Early throwaway notes can be plain markdown, but by the time a spec is being
refined for graduation it should read like a polished design doc — not prose.

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

## Diagrams

A `<Diagram>` MUST carry a non-empty, valid mermaid source — an empty or broken
diagram renders as an "Empty mermaid diagram" / "Syntax error" box and is
rejected at authoring time.

Put the mermaid as the block's **children** (between the tags):

```
<Diagram id="flow" type="mermaid">
flowchart LR
  A["Start"] --> B["Validate input"] --> C["Done"]
</Diagram>
```

- Use valid mermaid: a `flowchart` / `graph` header and ASCII `-->` edges.
- **ALWAYS quote node labels** — `A["clippy -D warnings"]`, not
  `A[clippy -D warnings]`. Unquoted `(`, `)`, `:`, `-`, or `/` break the parser.
  This is the #1 cause of "Syntax error" diagrams. Use `<br/>` for line breaks
  inside a quoted label.
- Keep it small — a handful of nodes beats a dense, unreadable graph.
- NEVER emit an empty `<Diagram>`. If you cannot express a concrete diagram,
  use prose or a list instead.

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
