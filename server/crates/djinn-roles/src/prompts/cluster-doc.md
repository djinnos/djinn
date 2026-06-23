# Cluster Doc Synthesis

You are summarizing one **community** of related code symbols inside a repository. The community was detected by greedy modularity-based clustering over the canonical dependency graph (PR F3). Your job is to produce a short, human-readable doc that helps a developer (or another agent) orient inside this module quickly.

## Community

- **Module name / label:** {{MODULE_NAME}}
- **Project:** {{PROJECT_INFO}}

## Internal calls (intra-community edges)

These are the edges connecting symbols **inside** this community. Use them as evidence of which symbols are central / which call which.

{{INTRA_CALLS}}

## Outgoing calls (edges leaving the community)

These edges hint at the integration points: who this community talks to elsewhere in the codebase.

{{OUTGOING_CALLS}}

## Top processes

If the canonical graph attaches `Process` flows to symbols in this community, the highest-weighted ones are listed below. Treat them as the "canonical happy paths" the community implements.

{{TOP_PROCESSES}}

## Children-cluster docs (parent pass only)

When this cluster is a parent of smaller sub-clusters, their already-generated docs are stitched in here. Synthesize from these summaries — do **not** re-read source. For leaf clusters this section is empty.

{{CHILDREN_DOCS}}

## Instructions

Synthesize a **200–400 word doc** summarizing this module. Cover:

1. **Purpose** — what problem this cluster solves in one sentence, then a paragraph of context.
2. **Key abstractions** — name the 3–5 most important symbols and what role each plays. Pull from the intra-call evidence.
3. **Integration points** — which other parts of the codebase this module collaborates with, based on the outgoing-calls list. Group by direction (callers vs. callees) when the signal is clear.
4. **Notable processes** — if the top-processes section is non-empty, mention the most important one or two flows by name.

Constraints:
- No fabricated APIs. If a symbol or process isn't in the lists above, don't invent it.
- No source-code blocks — the reader can click through. Keep it prose with bullets where helpful.
- Plain Markdown; no front-matter, no HTML, no tables wider than three columns.
- Open with a one-line tldr, then sections.
